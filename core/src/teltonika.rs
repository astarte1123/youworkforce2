// Edge nativo Teltonika — server TCP Codec8 / Codec8 Extended (0x08 / 0x8E).
// Decode-edge nativo: il device parla il protocollo binario diretto con YouWorkforce.
// Handshake IMEI -> accept (0x01) solo se l'IMEI = external_id di UN SOLO device `gps` (in tutto
// il sistema) con un modulo dispositivi attivo; IMEI ambiguo (in più tenant) = rifiutato; poi decodifica i pacchetti AVL e li immette nella pipeline nativa
// (api::teltonika_ingest_one). Identità = IMEI (il protocollo non porta token): accettiamo solo
// IMEI censiti. Rispondiamo con il numero di record accettati come da specifica.

use crate::state::AppState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{mpsc, oneshot};

// ---- Registry connessioni attive (per inviare comandi GPRS codec12 al device) ----
// ISOLAMENTO TENANT: la chiave è (tenant, IMEI), mai il solo IMEI. Un comando parte solo verso la
// connessione che l'handshake ha associato a QUEL tenant: un altro tenant che registra lo stesso
// IMEI non può raggiungere il socket. `conn_id` distingue le riconnessioni (la chiusura di una
// connessione vecchia non deve cancellare quella nuova). `send_lock` serializza i comandi sulla
// stessa connessione, così una risposta non viene mai consegnata al chiamante sbagliato.
type RegKey = (uuid::Uuid, String);
struct Conn {
    conn_id: u64,
    /// istante dell'handshake accettato (orologio dell'app): "collegato ora da …" nella scheda
    since: chrono::DateTime<chrono::Utc>,
    /// device risolto all'handshake: un invio verso un device diverso (cancellato e ricreato,
    /// spostato di tenant) non usa questa connessione
    device_id: uuid::Uuid,
    cmd: mpsc::UnboundedSender<Vec<u8>>,
    pending: Arc<Mutex<Option<oneshot::Sender<String>>>>,
    send_lock: Arc<tokio::sync::Mutex<()>>,
}
fn registry() -> &'static Mutex<HashMap<RegKey, Conn>> {
    static R: OnceLock<Mutex<HashMap<RegKey, Conn>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}
/// Il badge (tenant, IMEI) ha ADESSO una connessione TCP aperta? Ritorna da quando.
/// Serve alla scheda dispositivo per confermare il collegamento senza aspettare un fix GPS.
pub fn connected_since(tid: uuid::Uuid, imei: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    registry().lock().unwrap().get(&(tid, imei.to_string())).map(|c| c.since)
}
fn next_conn_id() -> u64 {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// CRC-16/IBM (poly 0xA001) sul data field, come richiesto dal protocollo Teltonika.
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data { crc ^= b as u16; for _ in 0..8 { if crc & 1 != 0 { crc = (crc >> 1) ^ 0xA001; } else { crc >>= 1; } } }
    crc
}
/// Codifica un comando GPRS (codec12, type 0x05) in un pacchetto completo.
fn encode_command(cmd: &str) -> Vec<u8> {
    let c = cmd.as_bytes();
    let mut inner = vec![0x0C, 0x01, 0x05];
    inner.extend_from_slice(&(c.len() as u32).to_be_bytes());
    inner.extend_from_slice(c);
    inner.push(0x01);
    let mut pkt = vec![0u8, 0, 0, 0];
    pkt.extend_from_slice(&(inner.len() as u32).to_be_bytes());
    pkt.extend_from_slice(&inner);
    pkt.extend_from_slice(&(crc16(&inner) as u32).to_be_bytes());
    pkt
}
/// Invia un comando GPRS al device `device_id` (tenant, IMEI) connesso e attende la risposta (max 5s).
/// Titolarità CORRENTE: la connessione deve appartenere a quel tenant E a quel device (risolti
/// all'handshake e ricontrollati a ogni pacchetto dal lettore); altrimenti "device non connesso".
pub async fn send_command(tid: uuid::Uuid, device_id: uuid::Uuid, imei: &str, cmd: &str) -> Result<String, String> {
    let key: RegKey = (tid, imei.to_string());
    let (conn_id, tx_cmd, pending, lock) = {
        let reg = registry().lock().unwrap();
        let conn = reg.get(&key).filter(|c| c.device_id == device_id)
            .ok_or_else(|| "device non connesso".to_string())?;
        (conn.conn_id, conn.cmd.clone(), conn.pending.clone(), conn.send_lock.clone())
    };
    // un comando alla volta per connessione: lo slot di risposta è unico
    let _guard = match tokio::time::timeout(std::time::Duration::from_secs(10), lock.lock_owned()).await {
        Ok(g) => g,
        Err(_) => return Err("dispositivo occupato con un altro comando".to_string()),
    };
    // la connessione potrebbe essere cambiata mentre si attendeva il turno
    let still = registry().lock().unwrap().get(&key).map(|c| c.conn_id) == Some(conn_id);
    if !still { return Err("device non connesso".to_string()); }
    let (tx, rx) = oneshot::channel();
    *pending.lock().unwrap() = Some(tx);
    if tx_cmd.send(encode_command(cmd)).is_err() {
        *pending.lock().unwrap() = None;
        return Err("connessione chiusa".to_string());
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
        Ok(Ok(resp)) => Ok(resp),
        _ => { *pending.lock().unwrap() = None; Err("nessuna risposta dal device entro 5s".to_string()) }
    }
}
/// Parsa una risposta codec12 (type 0x06): ritorna il testo.
fn parse_cmd_response(data: &[u8]) -> Option<String> {
    if data.len() < 8 || data[0] != 0x0C { return None; }
    // [0]=codec,[1]=qty1,[2]=type(0x06),[3..7]=size,[7..]=text,[..]=qty2
    let size = u32::from_be_bytes([data[3],data[4],data[5],data[6]]) as usize;
    if data.len() < 7 + size { return None; }
    Some(String::from_utf8_lossy(&data[7..7+size]).to_string())
}

/// Avvia il listener TCP se configurato. `YW_TELTONIKA_PORT` = "off" disabilita; default 5027.
pub async fn serve(st: AppState) {
    let port = std::env::var("YW_TELTONIKA_PORT").unwrap_or_else(|_| "5027".into());
    if port == "off" || port == "0" { tracing::info!("teltonika edge disabilitato"); return; }
    let addr = format!("0.0.0.0:{port}");
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => { tracing::warn!("teltonika: bind {addr} fallito: {e}"); return; }
    };
    tracing::info!("teltonika edge TCP in ascolto su {addr}");
    loop {
        match listener.accept().await {
            Ok((sock, peer)) => {
                let st = st.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(st, sock, peer.to_string()).await {
                        tracing::debug!("teltonika[{peer}]: chiusa ({e})");
                    }
                });
            }
            Err(e) => { tracing::warn!("teltonika accept: {e}"); tokio::time::sleep(std::time::Duration::from_millis(200)).await; }
        }
    }
}

async fn handle(st: AppState, mut sock: TcpStream, peer: String) -> std::io::Result<()> {
    // 1) handshake IMEI: [u16 len][ascii imei]
    let mut lenb = [0u8; 2];
    sock.read_exact(&mut lenb).await?;
    let ilen = u16::from_be_bytes(lenb) as usize;
    if ilen == 0 || ilen > 64 { let _ = sock.write_all(&[0x00]).await; return Ok(()); }
    let mut imei_buf = vec![0u8; ilen];
    sock.read_exact(&mut imei_buf).await?;
    let imei = String::from_utf8_lossy(&imei_buf).trim().to_string();

    // Identità: IMEI -> UN SOLO device `gps` in tutto il sistema. Sconosciuto o AMBIGUO (stesso
    // IMEI in più tenant) = rifiutato: non si associa mai automaticamente un'identità contesa.
    let (tid, did, source) = match crate::api::teltonika_resolve(&st, &imei).await {
        crate::api::ImeiMatch::One(t, d, s) if crate::api::any_device_module(&st, t).await => (t, d, s),
        crate::api::ImeiMatch::Ambiguous(n) => {
            let _ = sock.write_all(&[0x00]).await;
            tracing::warn!("teltonika: IMEI {imei} RIFIUTATO: registrato in {n} tenant (identità ambigua)");
            crate::api::record_connect_attempt(&st, "teltonika", &imei, "conteso", &peer).await;
            return Ok(());
        }
        crate::api::ImeiMatch::One(..) => {
            let _ = sock.write_all(&[0x00]).await;
            tracing::info!("teltonika: IMEI {imei} rifiutato: nessun modulo dispositivi attivo sul tenant");
            crate::api::record_connect_attempt(&st, "teltonika", &imei, "modulo_spento", &peer).await;
            return Ok(());
        }
        crate::api::ImeiMatch::None => {
            let _ = sock.write_all(&[0x00]).await;
            tracing::info!("teltonika: IMEI {imei} rifiutato: non censito");
            crate::api::record_connect_attempt(&st, "teltonika", &imei, "sconosciuto", &peer).await;
            return Ok(());
        }
    };
    crate::api::clear_connect_attempt(&st, "teltonika", &imei).await;
    // mappa IO->allarme del modello+tenant risolta UNA volta per connessione (salva-vita configurabile)
    let io_alarm_map = crate::api::teltonika_io_alarm_map(&st, tid, did).await;
    sock.write_all(&[0x01]).await?; // accept
    crate::api::touch_device_seen(&st, tid, &source, &imei).await;
    tracing::info!("teltonika: IMEI {imei} accettato (tenant {tid})");

    // registra la connessione per i comandi GPRS (codec12) e separa read/write
    let (mut rd, mut wr) = sock.into_split();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let pending: Arc<Mutex<Option<oneshot::Sender<String>>>> = Arc::new(Mutex::new(None));
    let conn_id = next_conn_id();
    let key: RegKey = (tid, imei.clone());
    registry().lock().unwrap().insert(key.clone(), Conn{ conn_id, since: st.clock.now(), device_id: did, cmd: cmd_tx.clone(), pending: pending.clone(),
        send_lock: Arc::new(tokio::sync::Mutex::new(())) });

    // writer task: scrive sul socket i comandi in coda e gli ACK dei record
    let writer = tokio::spawn(async move {
        while let Some(bytes) = cmd_rx.recv().await {
            if wr.write_all(&bytes).await.is_err() { break; }
        }
    });

    // reader loop: AVL (ingest) + risposte comando (codec12 type 0x06 -> fulfill pending)
    let res = read_loop(&st, tid, did, &imei, &source, io_alarm_map, &pending, &cmd_tx, &mut rd).await;
    {
        // rimuove SOLO la propria voce: una riconnessione più recente resta registrata
        let mut reg = registry().lock().unwrap();
        if reg.get(&key).map(|c| c.conn_id) == Some(conn_id) { reg.remove(&key); }
    }
    writer.abort();
    res
}

#[allow(clippy::too_many_arguments)]
async fn read_loop(st: &AppState, tid: uuid::Uuid, did: uuid::Uuid, imei: &str, source: &str,
    mut io_alarm_map: HashMap<i64,String>,
    pending: &Arc<Mutex<Option<oneshot::Sender<String>>>>,
    ack_tx: &mpsc::UnboundedSender<Vec<u8>>,
    rd: &mut tokio::net::tcp::OwnedReadHalf) -> std::io::Result<()> {
    // SL-16: la mappa IO -> allarme si rilegge ogni minuto (prima una volta per connessione: una
    // modifica della configurazione valeva solo dopo che il badge si ricollegava)
    let mut map_loaded = std::time::Instant::now();
    loop {
        if map_loaded.elapsed() > std::time::Duration::from_secs(60) {
            io_alarm_map = crate::api::teltonika_io_alarm_map(st, tid, did).await;
            map_loaded = std::time::Instant::now();
        }
        let mut head = [0u8; 8];
        match rd.read_exact(&mut head).await { Ok(_) => {}, Err(_) => return Ok(()) }
        let data_len = u32::from_be_bytes([head[4],head[5],head[6],head[7]]) as usize;
        if data_len == 0 || data_len > 1_000_000 { return Ok(()); }
        let mut buf = vec![0u8; data_len + 4]; // data + CRC16(4)
        rd.read_exact(&mut buf).await?;
        let data = &buf[..data_len];
        // risposta a comando GPRS (codec12 0x0C)
        if data.first() == Some(&0x0C) {
            if let Some(txt) = parse_cmd_response(data) {
                if let Some(tx) = pending.lock().unwrap().take() { let _ = tx.send(txt); }
            }
            continue;
        }
        // TITOLARITÀ CORRENTE prima di ingerire: se nel frattempo il device è stato cancellato,
        // spostato in un altro tenant o l'IMEI è diventato conteso, la connessione chiude senza
        // ACK. Il badge riconnette e l'handshake lo associa al titolare attuale (o lo rifiuta):
        // posizioni e SOS non finiscono mai al titolare precedente.
        if !matches!(crate::api::teltonika_resolve(st, imei).await,
                     crate::api::ImeiMatch::One(t, d, _) if t == tid && d == did) {
            tracing::warn!("teltonika: IMEI {imei} non più del tenant {tid}: connessione chiusa");
            return Ok(());
        }
        // SA-5: tenant sospeso (o tutti i moduli dispositivi spenti) dopo l'handshake -> chiude.
        if !crate::api::any_device_module(st, tid).await {
            tracing::warn!("teltonika: tenant {tid} sospeso o senza moduli dispositivi: connessione chiusa");
            return Ok(());
        }
        match decode_avl(data) {
            Ok(records) => {
                let n = records.len() as u32;
                // ultimo contatto: anche un pacchetto senza fix GPS prova che il device è vivo
                crate::api::touch_device_seen(st, tid, source, imei).await;
                for r in &records {
                    let ts_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(r.ts_ms)
                        .unwrap_or_else(chrono::Utc::now).to_rfc3339();
                    // SL-16: allarme solo quando l'IO dell'evento è ATTIVO (pulsante premuto, caduta
                    // rilevata); il record generato al rilascio (valore 0) non apre un altro allarme
                    let ev_val = r.io.iter().find(|(id, _)| *id == r.event_io).map(|x| x.1);
                    let alarm = if ev_val == Some(0) { None } else { alarm_for(&io_alarm_map, r.event_io) };
                    let io_attrs = io_attributes(&r.io);
                    crate::api::teltonika_ingest_one(st, tid, imei, source, &ts_rfc,
                        r.lat, r.lon, r.speed, r.course, r.battery, alarm, io_attrs).await;
                }
                // ACK = numero record accettati, sul canale del PROPRIO writer (mai via registry:
                // la voce potrebbe appartenere a una connessione più recente).
                let _ = ack_tx.send(n.to_be_bytes().to_vec());
            }
            Err(e) => {
                tracing::debug!("teltonika: decode errore ({e})");
                let _ = ack_tx.send(0u32.to_be_bytes().to_vec());
            }
        }
    }
}

/// event IO id -> stringa allarme. Mappa di DEFAULT built-in per gli IO Teltonika più comuni,
/// con override opzionale per-tenant (config teltonika.io_alarm_map, se il catalogo la espone).
fn alarm_for(io_alarm_map: &HashMap<i64,String>, event_io: i64) -> Option<String> {
    if event_io == 0 { return None; }
    // mappa modello+tenant (vince sul built-in): es. GH-5200 236->sos, 242->man_down
    if let Some(v) = io_alarm_map.get(&event_io) { return Some(v.clone()); }
    // default built-in per gli IO Teltonika più comuni
    let def = match event_io {
        1   => "sos",      // DIN1 / pulsante panico (cablaggio tipico)
        242 => "man_down", // ManDown/FallDown (tracker personali GH/TMT) — uomo a terra / caduta
        236 => "alarm",    // "Alarm" generico (per i personali la mappa-modello lo porta a 'sos')
        247 => "crash",    // Crash detection
        246 => "tow",      // Towing
        _   => return None,
    };
    Some(def.to_string())
}

#[derive(Debug)]
pub struct Rec { pub ts_ms: i64, pub lat: f64, pub lon: f64, pub speed: Option<f64>, pub course: Option<f64>, pub battery: Option<f64>, pub event_io: i64, pub io: Vec<(i64,i64)> }

/// Converte gli IO AVL in attributi: mappa grezza `io:{id:val}` + alcuni nomi noti Teltonika.
fn io_attributes(io: &[(i64,i64)]) -> serde_json::Value {
    let mut raw = serde_json::Map::new();
    let mut named = serde_json::Map::new();
    for &(id, val) in io {
        raw.insert(id.to_string(), serde_json::json!(val));
        let name = match id {
            66 => Some("ext_voltage_mv"), 67 => Some("battery_voltage_mv"),
            21 => Some("gsm_signal"), 69 => Some("gnss_status"), 200 => Some("sleep_mode"),
            240 => Some("movement"), 16 => Some("odometer_total_m"), 199 => Some("trip_odometer_m"),
            1 => Some("din1"), 2 => Some("din2"), 3 => Some("din3"),
            179 => Some("dout1"), 180 => Some("dout2"),
            72 => Some("temp1_dc"), 73 => Some("temp2_dc"), 74 => Some("temp3_dc"),
            _ => None,
        };
        if let Some(n) = name { named.insert(n.to_string(), serde_json::json!(val)); }
    }
    named.insert("io".to_string(), serde_json::Value::Object(raw));
    serde_json::Value::Object(named)
}

struct Cur<'a> { b: &'a [u8], i: usize }
impl<'a> Cur<'a> {
    fn new(b: &'a [u8]) -> Self { Cur { b, i: 0 } }
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.i + n > self.b.len() { return Err("buffer troncato".into()); }
        let s = &self.b[self.i..self.i+n]; self.i += n; Ok(s)
    }
    fn u8(&mut self) -> Result<u8,String> { Ok(self.take(1)?[0]) }
    fn u16(&mut self) -> Result<u16,String> { let s=self.take(2)?; Ok(u16::from_be_bytes([s[0],s[1]])) }
    fn u32(&mut self) -> Result<u32,String> { let s=self.take(4)?; Ok(u32::from_be_bytes([s[0],s[1],s[2],s[3]])) }
    fn i32(&mut self) -> Result<i32,String> { Ok(self.u32()? as i32) }
    fn u64(&mut self) -> Result<u64,String> { let s=self.take(8)?; let mut a=[0u8;8]; a.copy_from_slice(s); Ok(u64::from_be_bytes(a)) }
}

/// Decodifica un data field AVL (codec id ... num record finale). Supporta Codec8 (0x08) e 8E (0x8E).
pub fn decode_avl(data: &[u8]) -> Result<Vec<Rec>, String> {
    let mut c = Cur::new(data);
    let codec = c.u8()?;
    let ext = match codec { 0x08 => false, 0x8E => true, other => return Err(format!("codec {other:#x} non supportato")) };
    let count = c.u8()? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let ts_ms = c.u64()? as i64;
        let _priority = c.u8()?;
        // GPS element
        let lon = c.i32()? as f64 / 1e7;
        let lat = c.i32()? as f64 / 1e7;
        let _alt = c.u16()?;
        let angle = c.u16()? as f64;
        let _sat = c.u8()?;
        let speed = c.u16()? as f64;
        // IO element
        let event_io = if ext { c.u16()? as i64 } else { c.u8()? as i64 };
        let _total = if ext { c.u16()? as usize } else { c.u8()? as usize };
        let mut battery: Option<f64> = None;
        let mut io: Vec<(i64,i64)> = Vec::new();
        let mut read_set = |c: &mut Cur, vlen: usize| -> Result<(),String> {
            let n = if ext { c.u16()? as usize } else { c.u8()? as usize };
            for _ in 0..n {
                let id = if ext { c.u16()? as i64 } else { c.u8()? as i64 };
                let raw = c.take(vlen)?;
                let mut val: i64 = 0; for &x in raw { val = (val << 8) | x as i64; }
                // IO 113 = livello batteria % (FMB/GH). Fallback: 68 battery current ignorato.
                if id == 113 { battery = Some(val as f64); }
                io.push((id, val));
            }
            Ok(())
        };
        read_set(&mut c, 1)?;
        read_set(&mut c, 2)?;
        read_set(&mut c, 4)?;
        read_set(&mut c, 8)?;
        if ext {
            // sezione NX (variabile): [u16 n] poi n * ([u16 id][u16 len][len byte])
            let nx = c.u16()? as usize;
            for _ in 0..nx { let _id = c.u16()?; let l = c.u16()? as usize; let _ = c.take(l)?; }
        }
        out.push(Rec{ ts_ms, lat, lon, speed: Some(speed), course: Some(angle), battery, event_io, io });
    }
    let count2 = c.u8()? as usize;
    if count2 != count { return Err(format!("mismatch record count {count} != {count2}")); }
    Ok(out)
}

#[cfg(test)]
mod sl16_tests {
    #[test]
    fn release_value_is_not_an_alarm() {
        let io = vec![(1i64, 0i64), (113, 80)];
        let ev_val = io.iter().find(|(id, _)| *id == 1).map(|x| x.1);
        assert_eq!(ev_val, Some(0));
        let io2 = vec![(1i64, 1i64)];
        assert_eq!(io2.iter().find(|(id, _)| *id == 1).map(|x| x.1), Some(1));
    }
}
