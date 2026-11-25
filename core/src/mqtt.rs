// Broker MQTT nativo YW — MQTT 3.1.1 minimale su TCP (porta 1883) e, se configurato, su TLS
// (porta 8883), SENZA broker esterno.
// Perimetro IoT: i sensori pubblicano la telemetria su `yw/{tid}/{ext}/telemetry`; il broker
// autentica con l'ingest token (username della CONNECT) e immette nella STESSA pipeline nativa
// dell'HTTP (api::mqtt_ingest -> persist_telemetry: normalizzazione + soglie + incidenti).
//
// ISOLAMENTO MULTI-TENANT: il tenant è SEMPRE quello del token; se il topic porta un tenant,
// deve combaciare (api::mqtt_topic_ext). Un client non può pubblicare per un tenant altrui.
//
// IO-11 (25/11/2026):
// - TLS opzionale: YW_MQTT_TLS_CERT + YW_MQTT_TLS_KEY (PEM) aprono la porta YW_MQTTS_PORT
//   (default 8883); con YW_MQTT_PLAIN=off la porta in chiaro non si apre;
// - limiti: connessioni contemporanee (YW_MQTT_MAX_CONN, default 500), CONNECT entro 10 s,
//   keepalive applicato (1,5 × il valore del client; senza keepalive, 10 minuti di inattività);
// - QoS 1: PUBACK SOLO se il dato è stato accettato; un dato scartato chiude la connessione (il
//   client sa che non è arrivato, invece di una conferma falsa);
// - comandi: consegnati anche MENTRE il client è collegato (controllo ogni 5 s), non solo alla
//   riconnessione.
// Scope volutamente ristretto (no QoS2, no retained/persistenza, no will).

use crate::state::AppState;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::{timeout, Duration};

const CONNECT: u8 = 1;
const PUBLISH: u8 = 3;
const SUBSCRIBE: u8 = 8;
const PINGREQ: u8 = 12;
const DISCONNECT: u8 = 14;

static ACTIVE: AtomicUsize = AtomicUsize::new(0);
fn max_conn() -> usize { std::env::var("YW_MQTT_MAX_CONN").ok().and_then(|v| v.parse().ok()).unwrap_or(500) }
/// Connessioni MQTT aperte in questo momento (pannello di sistema / test).
pub fn active_connections() -> usize { ACTIVE.load(Ordering::Relaxed) }

/// Avvia i listener configurati. `YW_MQTT_PORT` = "off"/"0" disabilita il chiaro; default 1883.
pub async fn serve(st: AppState) {
    if let Some(acceptor) = tls_acceptor() {
        let port = std::env::var("YW_MQTTS_PORT").unwrap_or_else(|_| "8883".into());
        let st2 = st.clone();
        tokio::spawn(async move {
            let addr = format!("0.0.0.0:{port}");
            match TcpListener::bind(&addr).await {
                Ok(l) => { tracing::info!("mqtt broker nativo TLS in ascolto su {addr}"); accept_loop(st2, l, Some(acceptor)).await; }
                Err(e) => tracing::warn!("mqtt tls: bind {addr} fallito: {e}"),
            }
        });
    }
    let plain_off = std::env::var("YW_MQTT_PLAIN").ok().as_deref() == Some("off");
    let port = std::env::var("YW_MQTT_PORT").unwrap_or_else(|_| "1883".into());
    if plain_off || port == "off" || port == "0" { tracing::info!("mqtt in chiaro disabilitato"); return; }
    let addr = format!("0.0.0.0:{port}");
    match TcpListener::bind(&addr).await {
        Ok(l) => { tracing::info!("mqtt broker nativo in ascolto su {addr}"); accept_loop(st, l, None).await; }
        Err(e) => tracing::warn!("mqtt: bind {addr} fallito: {e}"),
    }
}

fn tls_acceptor() -> Option<tokio_rustls::TlsAcceptor> {
    use rustls_pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
    let (cp, kp) = (std::env::var("YW_MQTT_TLS_CERT").ok()?, std::env::var("YW_MQTT_TLS_KEY").ok()?);
    let certs: Vec<CertificateDer<'static>> = match CertificateDer::pem_file_iter(&cp) {
        Ok(it) => it.filter_map(|c| c.ok()).collect(), Err(e) => { tracing::warn!("mqtt tls: certificato {cp}: {e}"); return None; } };
    let key = match PrivateKeyDer::from_pem_file(&kp) { Ok(k) => k, Err(e) => { tracing::warn!("mqtt tls: chiave {kp}: {e}"); return None; } };
    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let cfg = tokio_rustls::rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions().ok()?
        .with_no_client_auth().with_single_cert(certs, key);
    match cfg { Ok(c) => Some(tokio_rustls::TlsAcceptor::from(Arc::new(c))), Err(e) => { tracing::warn!("mqtt tls: configurazione: {e}"); None } }
}

async fn accept_loop(st: AppState, listener: TcpListener, tls: Option<tokio_rustls::TlsAcceptor>) {
    loop {
        match listener.accept().await {
            Ok((sock, peer)) => {
                if ACTIVE.load(Ordering::Relaxed) >= max_conn() {
                    tracing::warn!("mqtt: limite connessioni raggiunto, rifiuto {peer}");
                    drop(sock); continue;
                }
                sock.set_nodelay(true).ok();
                let st = st.clone(); let tls = tls.clone();
                tokio::spawn(async move {
                    ACTIVE.fetch_add(1, Ordering::Relaxed);
                    let r = match tls {
                        Some(acc) => match timeout(Duration::from_secs(10), acc.accept(sock)).await {
                            Ok(Ok(s)) => handle(st, s).await,
                            _ => Ok(()),
                        },
                        None => handle(st, sock).await,
                    };
                    ACTIVE.fetch_sub(1, Ordering::Relaxed);
                    if let Err(e) = r { tracing::debug!("mqtt[{peer}]: chiusa ({e})"); }
                });
            }
            Err(e) => { tracing::warn!("mqtt accept: {e}"); tokio::time::sleep(Duration::from_millis(200)).await; }
        }
    }
}

// ---- varint remaining length ----
fn encode_remaining_length(mut len: usize, out: &mut Vec<u8>) {
    loop {
        let mut b = (len % 128) as u8;
        len /= 128;
        if len > 0 { b |= 0x80; }
        out.push(b);
        if len == 0 { break; }
    }
}
async fn read_remaining_length<R: AsyncRead + Unpin>(rd: &mut R) -> std::io::Result<usize> {
    let mut mult = 1usize; let mut value = 0usize;
    for _ in 0..4 {
        let b = rd.read_u8().await?;
        value += (b & 0x7f) as usize * mult;
        if b & 0x80 == 0 { return Ok(value); }
        mult *= 128;
    }
    Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "remaining length malformata"))
}

// ---- cursore sul body del pacchetto ----
struct Cur<'a> { b: &'a [u8], i: usize }
impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self { Cur { b, i: 0 } }
    fn rem(&self) -> usize { self.b.len().saturating_sub(self.i) }
    fn u8(&mut self) -> Option<u8> { let v = *self.b.get(self.i)?; self.i += 1; Some(v) }
    fn u16(&mut self) -> Option<u16> { if self.rem() < 2 { return None; } let v = u16::from_be_bytes([self.b[self.i], self.b[self.i+1]]); self.i += 2; Some(v) }
    /// stringa MQTT: [u16 len][utf8]
    fn mstr(&mut self) -> Option<String> { let n = self.u16()? as usize; if self.rem() < n { return None; } let s = String::from_utf8_lossy(&self.b[self.i..self.i+n]).to_string(); self.i += n; Some(s) }
    fn rest(&self) -> &'a [u8] { &self.b[self.i.min(self.b.len())..] }
}

async fn send<W: AsyncWrite + Unpin>(wr: &mut W, type_flags: u8, body: &[u8]) -> std::io::Result<()> {
    let mut pkt = vec![type_flags];
    encode_remaining_length(body.len(), &mut pkt);
    pkt.extend_from_slice(body);
    // un client che non legge più riempie il buffer TCP: senza limite la scrittura resterebbe
    // bloccata per sempre e terrebbe occupato uno slot di connessione
    match timeout(Duration::from_secs(15), wr.write_all(&pkt)).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "scrittura MQTT scaduta")),
    }
}

async fn handle<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(st: AppState, sock: S) -> std::io::Result<()> {
    let (mut rd, mut wr) = tokio::io::split(sock);

    // 1) deve arrivare una CONNECT per prima, entro 10 s
    let (ptype, _flags, body) = match timeout(Duration::from_secs(10), read_packet(&mut rd)).await {
        Ok(r) => r?, Err(_) => return Ok(()) };
    if ptype != CONNECT { return Ok(()); } // protocollo violato: chiudi
    let mut c = Cur::new(&body);
    let proto = c.mstr().unwrap_or_default();
    if proto != "MQTT" && proto != "MQIsdp" {
        // CONNACK 0x01 (protocollo non supportato) e chiudi
        let _ = send(&mut wr, 0x20, &[0x00, 0x01]).await; return Ok(());
    }
    let _level = c.u8();
    let conn_flags = c.u8().unwrap_or(0);
    let keepalive = c.u16().unwrap_or(0);
    let _client_id = c.mstr();
    // salta will se presente
    if conn_flags & 0x04 != 0 { c.mstr(); c.mstr(); }
    let username = if conn_flags & 0x80 != 0 { c.mstr() } else { None };
    let _password = if conn_flags & 0x40 != 0 { c.mstr() } else { None };

    // 2) autenticazione: username = ingest token
    let token = match username { Some(u) if !u.is_empty() => u, _ => { let _ = send(&mut wr, 0x20, &[0x00, 0x04]).await; return Ok(()); } };
    let (tid, bound) = match crate::api::mqtt_auth(&st, &token).await {
        Ok(x) => x,
        Err(e) => { tracing::debug!("mqtt: CONNECT rifiutata ({e})"); let _ = send(&mut wr, 0x20, &[0x00, 0x05]).await; return Ok(()); } // 0x05 not authorized
    };
    send(&mut wr, 0x20, &[0x00, 0x00]).await?; // CONNACK accepted
    tracing::info!("mqtt: client connesso (tenant {tid})");

    // lettore separato (le letture parziali non si perdono quando scatta il timer dei comandi)
    let (tx, mut rx) = tokio::sync::mpsc::channel::<std::io::Result<(u8, u8, Vec<u8>)>>(16);
    let reader = tokio::spawn(async move {
        loop { let r = read_packet(&mut rd).await; let end = r.is_err(); if tx.send(r).await.is_err() || end { break; } }
    });
    // keepalive: 1,5 × il valore dichiarato (spec MQTT); senza keepalive, 10 minuti di inattività
    let idle = if keepalive > 0 { Duration::from_millis(keepalive as u64 * 1500) } else { Duration::from_secs(600) };
    let mut cmd_filters: Vec<String> = Vec::new();
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    // scadenza unica, spostata solo quando arriva un pacchetto (il timer dei comandi non la azzera)
    let deadline = tokio::time::sleep(idle);
    tokio::pin!(deadline);
    let res: std::io::Result<()> = async {
        loop {
            tokio::select! {
                _ = &mut deadline => { tracing::debug!("mqtt: keepalive scaduto (tenant {tid})"); return Ok(()); }
                pkt = rx.recv() => {
                    deadline.as_mut().reset(tokio::time::Instant::now() + idle);
                    let (ptype, flags, body) = match pkt {
                        None | Some(Err(_)) => return Ok(()),
                        Some(Ok(p)) => p,
                    };
                    match ptype {
                        PUBLISH => {
                            let qos = (flags >> 1) & 0x03;
                            let mut c = Cur::new(&body);
                            let topic = match c.mstr() { Some(t) => t, None => return Ok(()) };
                            let pid = if qos > 0 { c.u16() } else { None };
                            let payload = c.rest();
                            match crate::api::mqtt_ingest(&st, tid, bound, &topic, payload).await {
                                Ok(n) => {
                                    tracing::debug!("mqtt: ingest {n} su {topic}");
                                    if qos == 1 { if let Some(id) = pid { send(&mut wr, 0x40, &id.to_be_bytes()).await?; } }
                                }
                                Err(e) => {
                                    // dato scartato: nessuna conferma falsa, si chiude
                                    tracing::debug!("mqtt: ingest scartato su {topic} ({e}): connessione chiusa");
                                    return Ok(());
                                }
                            }
                        }
                        SUBSCRIBE => {
                            // variable header: packet id; payload: ripetuto [topic filter][qos]
                            let mut c = Cur::new(&body);
                            let pid = c.u16().unwrap_or(0);
                            let mut filters: Vec<String> = Vec::new();
                            let mut granted: Vec<u8> = Vec::new();
                            while c.rem() >= 2 {
                                let f = match c.mstr() { Some(f) => f, None => break };
                                let _rq = c.u8().unwrap_or(0);
                                granted.push(0x00); // concediamo QoS0
                                filters.push(f);
                            }
                            // SUBACK: [pid][return codes]
                            let mut sb = pid.to_be_bytes().to_vec();
                            sb.extend_from_slice(&granted);
                            send(&mut wr, 0x90, &sb).await?;
                            for f in filters { if f.ends_with("/command") && !cmd_filters.contains(&f) { cmd_filters.push(f); } }
                            flush(&st, tid, bound, &cmd_filters, &mut wr).await?;
                        }
                        PINGREQ => { send(&mut wr, 0xD0, &[]).await?; } // PINGRESP
                        DISCONNECT => return Ok(()),
                        _ => { /* UNSUBSCRIBE/PUBACK ecc.: ignora */ }
                    }
                }
                _ = tick.tick() => { if !cmd_filters.is_empty() { flush(&st, tid, bound, &cmd_filters, &mut wr).await?; } }
            }
        }
    }.await;
    reader.abort();
    res
}

/// Pubblica i comandi pending per i filtri sottoscritti e li marca inviati.
async fn flush<W: AsyncWrite + Unpin>(st: &AppState, tid: uuid::Uuid, bound: Option<uuid::Uuid>, filters: &[String], wr: &mut W) -> std::io::Result<()> {
    for f in filters {
        match crate::api::mqtt_flush_commands(st, tid, bound, f).await {
            Ok(cmds) => for (i, cmd) in cmds.iter().enumerate() {
                let payload = serde_json::to_vec(cmd).unwrap_or_default();
                if let Err(e) = publish_qos0(wr, f, &payload).await {
                    // non scritti: tornano in coda per la prossima connessione
                    let ids: Vec<uuid::Uuid> = cmds[i..].iter()
                        .filter_map(|c| c.get("id").and_then(|v| v.as_str()).and_then(|s| s.parse().ok())).collect();
                    crate::api::mqtt_commands_unsent(st, tid, &ids).await;
                    return Err(e);
                }
            },
            Err(e) => tracing::debug!("mqtt: flush comandi scartato su {f} ({e})"),
        }
    }
    Ok(())
}

/// Legge un pacchetto completo: ritorna (tipo, flags, body).
async fn read_packet<R: AsyncRead + Unpin>(rd: &mut R) -> std::io::Result<(u8, u8, Vec<u8>)> {
    let b0 = rd.read_u8().await?;
    let rem = read_remaining_length(rd).await?;
    if rem > 2_000_000 { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "pacchetto troppo grande")); }
    let mut body = vec![0u8; rem];
    if rem > 0 { rd.read_exact(&mut body).await?; }
    Ok((b0 >> 4, b0 & 0x0f, body))
}

/// Pubblica una PUBLISH QoS0 verso il client (downlink comando).
async fn publish_qos0<W: AsyncWrite + Unpin>(wr: &mut W, topic: &str, payload: &[u8]) -> std::io::Result<()> {
    let mut body = Vec::with_capacity(2 + topic.len() + payload.len());
    body.extend_from_slice(&(topic.len() as u16).to_be_bytes());
    body.extend_from_slice(topic.as_bytes());
    body.extend_from_slice(payload);
    send(wr, 0x30, &body).await // PUBLISH, QoS0
}
