// Broker MQTT nativo YW — MQTT 3.1.1 minimale su TCP (porta 1883), SENZA broker esterno.
// Perimetro IoT: i sensori pubblicano la telemetria su `yw/{tid}/{ext}/telemetry`; il broker
// autentica con l'ingest token (username della CONNECT) e immette nella STESSA pipeline nativa
// dell'HTTP (api::mqtt_ingest -> persist_telemetry: normalizzazione + soglie + incidenti).
//
// ISOLAMENTO MULTI-TENANT: il tenant è SEMPRE quello del token; se il topic porta un tenant,
// deve combaciare (api::mqtt_topic_ext). Un client non può pubblicare per un tenant altrui.
//
// Scope volutamente ristretto (no QoS2, no retained/persistenza, no will): basta per l'ingest
// di sensori e il downlink dei comandi pending su SUBSCRIBE a `yw/{tid}/{ext}/command`.

use crate::state::AppState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

const CONNECT: u8 = 1;
const PUBLISH: u8 = 3;
const SUBSCRIBE: u8 = 8;
const PINGREQ: u8 = 12;
const DISCONNECT: u8 = 14;

/// Avvia il listener TCP se configurato. `YW_MQTT_PORT` = "off"/"0" disabilita; default 1883.
pub async fn serve(st: AppState) {
    let port = std::env::var("YW_MQTT_PORT").unwrap_or_else(|_| "1883".into());
    if port == "off" || port == "0" { tracing::info!("mqtt broker disabilitato"); return; }
    let addr = format!("0.0.0.0:{port}");
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => { tracing::warn!("mqtt: bind {addr} fallito: {e}"); return; }
    };
    tracing::info!("mqtt broker nativo in ascolto su {addr}");
    loop {
        match listener.accept().await {
            Ok((sock, peer)) => {
                let st = st.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(st, sock).await { tracing::debug!("mqtt[{peer}]: chiusa ({e})"); }
                });
            }
            Err(e) => { tracing::warn!("mqtt accept: {e}"); tokio::time::sleep(std::time::Duration::from_millis(200)).await; }
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
async fn read_remaining_length(rd: &mut OwnedReadHalf) -> std::io::Result<usize> {
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

async fn send(wr: &mut OwnedWriteHalf, type_flags: u8, body: &[u8]) -> std::io::Result<()> {
    let mut pkt = vec![type_flags];
    encode_remaining_length(body.len(), &mut pkt);
    pkt.extend_from_slice(body);
    wr.write_all(&pkt).await
}

async fn handle(st: AppState, sock: TcpStream) -> std::io::Result<()> {
    sock.set_nodelay(true).ok();
    let (mut rd, mut wr) = sock.into_split();

    // 1) deve arrivare una CONNECT per prima
    let (ptype, _flags, body) = read_packet(&mut rd).await?;
    if ptype != CONNECT { return Ok(()); } // protocollo violato: chiudi
    let mut c = Cur::new(&body);
    let proto = c.mstr().unwrap_or_default();
    if proto != "MQTT" && proto != "MQIsdp" {
        // CONNACK 0x01 (protocollo non supportato) e chiudi
        let _ = send(&mut wr, 0x20, &[0x00, 0x01]).await; return Ok(());
    }
    let _level = c.u8();
    let conn_flags = c.u8().unwrap_or(0);
    let _keepalive = c.u16();
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

    // 3) loop pacchetti
    loop {
        let (ptype, flags, body) = match read_packet(&mut rd).await { Ok(p) => p, Err(_) => return Ok(()) };
        match ptype {
            PUBLISH => {
                let qos = (flags >> 1) & 0x03;
                let mut c = Cur::new(&body);
                let topic = match c.mstr() { Some(t) => t, None => return Ok(()) };
                let pid = if qos > 0 { c.u16() } else { None };
                let payload = c.rest();
                match crate::api::mqtt_ingest(&st, tid, bound, &topic, payload).await {
                    Ok(n) => tracing::debug!("mqtt: ingest {n} su {topic}"),
                    Err(e) => tracing::debug!("mqtt: ingest scartato su {topic} ({e})"),
                }
                // PUBACK per QoS1 (si conferma la ricezione a prescindere dall'esito applicativo)
                if qos == 1 { if let Some(id) = pid { send(&mut wr, 0x40, &id.to_be_bytes()).await?; } }
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
                // downlink: per ogni filtro di comando, pubblica i comandi pending e marcali 'sent'
                for f in &filters {
                    if !f.ends_with("/command") { continue; }
                    match crate::api::mqtt_flush_commands(&st, tid, bound, f).await {
                        Ok(cmds) => for cmd in cmds {
                            let payload = serde_json::to_vec(&cmd).unwrap_or_default();
                            publish_qos0(&mut wr, f, &payload).await?;
                        },
                        Err(e) => tracing::debug!("mqtt: flush comandi scartato su {f} ({e})"),
                    }
                }
            }
            PINGREQ => { send(&mut wr, 0xD0, &[]).await?; } // PINGRESP
            DISCONNECT => return Ok(()),
            _ => { /* UNSUBSCRIBE/PUBACK ecc.: ignora */ }
        }
    }
}

/// Legge un pacchetto completo: ritorna (tipo, flags, body).
async fn read_packet(rd: &mut OwnedReadHalf) -> std::io::Result<(u8, u8, Vec<u8>)> {
    let b0 = rd.read_u8().await?;
    let rem = read_remaining_length(rd).await?;
    if rem > 2_000_000 { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "pacchetto troppo grande")); }
    let mut body = vec![0u8; rem];
    if rem > 0 { rd.read_exact(&mut body).await?; }
    Ok((b0 >> 4, b0 & 0x0f, body))
}

/// Pubblica una PUBLISH QoS0 verso il client (downlink comando).
async fn publish_qos0(wr: &mut OwnedWriteHalf, topic: &str, payload: &[u8]) -> std::io::Result<()> {
    let mut body = Vec::with_capacity(2 + topic.len() + payload.len());
    body.extend_from_slice(&(topic.len() as u16).to_be_bytes());
    body.extend_from_slice(topic.as_bytes());
    body.extend_from_slice(payload);
    send(wr, 0x30, &body).await // PUBLISH, QoS0
}
