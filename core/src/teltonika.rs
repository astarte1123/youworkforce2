// Edge nativo Teltonika — server TCP Codec8 / Codec8 Extended (0x08 / 0x8E).
// Decode-edge SENZA server Traccar: il device parla il protocollo binario diretto.
// Handshake IMEI -> accept (0x01) solo se l'IMEI = external_id di un device noto con un modulo
// dispositivi attivo; poi decodifica i pacchetti AVL e li immette nella pipeline nativa
// (api::teltonika_ingest_one). Identità = IMEI (il protocollo non porta token): accettiamo solo
// IMEI censiti. Rispondiamo con il numero di record accettati come da specifica.

use crate::state::AppState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{mpsc, oneshot};

// ---- Registry connessioni attive (per inviare comandi GPRS codec12 al device) ----
struct Conn { cmd: mpsc::UnboundedSender<Vec<u8>>, pending: Arc<Mutex<Option<oneshot::Sender<String>>>> }
fn registry() -> &'static Mutex<HashMap<String, Conn>> {
    static R: OnceLock<Mutex<HashMap<String, Conn>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
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
/// Invia un comando GPRS a un device connesso (per IMEI) e attende la risposta (max 5s).
pub async fn send_command(imei: &str, cmd: &str) -> Result<String, String> {
    let (tx, rx) = oneshot::channel();
    {
        let reg = registry().lock().unwrap();
        let conn = reg.get(imei).ok_or_else(|| "device non connesso".to_string())?;
        *conn.pending.lock().unwrap() = Some(tx);
        conn.cmd.send(encode_command(cmd)).map_err(|_| "connessione chiusa".to_string())?;
    }
    match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
        Ok(Ok(resp)) => Ok(resp),
        _ => Err("nessuna risposta dal device entro 5s".to_string()),
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
                    if let Err(e) = handle(st, sock).await {
                        tracing::debug!("teltonika[{peer}]: chiusa ({e})");
                    }
                });
            }
            Err(e) => { tracing::warn!("teltonika accept: {e}"); tokio::time::sleep(std::time::Duration::from_millis(200)).await; }
        }
    }
}

async fn handle(st: AppState, mut sock: TcpStream) -> std::io::Result<()> {
    // 1) handshake IMEI: [u16 len][ascii imei]
    let mut lenb = [0u8; 2];
    sock.read_exact(&mut lenb).await?;
    let ilen = u16::from_be_bytes(lenb) as usize;
    if ilen == 0 || ilen > 64 { let _ = sock.write_all(&[0x00]).await; return Ok(()); }
    let mut imei_buf = vec![0u8; ilen];
    sock.read_exact(&mut imei_buf).await?;
    let imei = String::from_utf8_lossy(&imei_buf).trim().to_string();

    let resolved = crate::api::teltonika_resolve(&st, &imei).await;
    let (tid, _did, source) = match resolved {
        Some(x) if crate::api::any_device_module(&st, x.0).await => x,
        _ => { let _ = sock.write_all(&[0x00]).await; tracing::debug!("teltonika: IMEI {imei} rifiutato"); return Ok(()); }
    };
    sock.write_all(&[0x01]).await?; // accept
    tracing::info!("teltonika: IMEI {imei} accettato (tenant {tid})");

    // registra la connessione per i comandi GPRS (codec12) e separa read/write
    let (mut rd, mut wr) = sock.into_split();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let pending: Arc<Mutex<Option<oneshot::Sender<String>>>> = Arc::new(Mutex::new(None));
    registry().lock().unwrap().insert(imei.clone(), Conn{ cmd: cmd_tx, pending: pending.clone() });

    // writer task: scrive sul socket i comandi in coda
    let writer = tokio::spawn(async move {
        while let Some(bytes) = cmd_rx.recv().await {
            if wr.write_all(&bytes).await.is_err() { break; }
        }
    });

    // reader loop: AVL (ingest) + risposte comando (codec12 type 0x06 -> fulfill pending)
    let res = read_loop(&st, tid, &imei, &source, &pending, &mut rd).await;
    registry().lock().unwrap().remove(&imei);
    writer.abort();
    res
}

async fn read_loop(st: &AppState, tid: uuid::Uuid, imei: &str, source: &str,
    pending: &Arc<Mutex<Option<oneshot::Sender<String>>>>,
    rd: &mut tokio::net::tcp::OwnedReadHalf) -> std::io::Result<()> {
    loop {
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
        match decode_avl(data) {
            Ok(records) => {
                let n = records.len() as u32;
                for r in &records {
                    let ts_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(r.ts_ms)
                        .unwrap_or_else(chrono::Utc::now).to_rfc3339();
                    let alarm = alarm_for(st, tid, r.event_io).await;
                    let io_attrs = io_attributes(&r.io);
                    crate::api::teltonika_ingest_one(st, tid, imei, source, &ts_rfc,
                        r.lat, r.lon, r.speed, r.course, r.battery, alarm, io_attrs).await;
                }
                // writer non è qui: l'ack va scritto dal reader? separiamo: scriviamo l'ack via canale
                // tramite registry (stesso writer task).
                if let Some(conn) = registry().lock().unwrap().get(imei) {
                    let _ = conn.cmd.send(n.to_be_bytes().to_vec());
                }
            }
            Err(e) => {
                tracing::debug!("teltonika: decode errore ({e})");
                if let Some(conn) = registry().lock().unwrap().get(imei) {
                    let _ = conn.cmd.send(0u32.to_be_bytes().to_vec());
                }
            }
        }
    }
}

/// event IO id -> stringa allarme. Mappa di DEFAULT built-in per gli IO Teltonika più comuni,
/// con override opzionale per-tenant (config teltonika.io_alarm_map, se il catalogo la espone).
async fn alarm_for(st: &AppState, tid: uuid::Uuid, event_io: i64) -> Option<String> {
    if event_io == 0 { return None; }
    // override tenant (vince sul default)
    let cfg = crate::api::tool_config(st, tid, "teltonika", "io_alarm_map").await;
    if let Some(v) = cfg.get(event_io.to_string()).and_then(|v| v.as_str()) { return Some(v.to_string()); }
    // default built-in
    let def = match event_io {
        1   => "sos",    // DIN1 / pulsante panico (cablaggio tipico)
        236 => "alarm",  // Teltonika "Alarm"
        247 => "crash",  // Crash detection
        246 => "tow",    // Towing
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
