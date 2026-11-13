// Edge nativo Teltonika — server TCP Codec8 / Codec8 Extended (0x08 / 0x8E).
// Decode-edge SENZA server Traccar: il device parla il protocollo binario diretto.
// Handshake IMEI -> accept (0x01) solo se l'IMEI = external_id di un device noto con un modulo
// dispositivi attivo; poi decodifica i pacchetti AVL e li immette nella pipeline nativa
// (api::teltonika_ingest_one). Identità = IMEI (il protocollo non porta token): accettiamo solo
// IMEI censiti. Rispondiamo con il numero di record accettati come da specifica.

use crate::state::AppState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

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

    // 2) loop pacchetti AVL
    loop {
        // preamble 4 byte (zero) + data length 4 byte
        let mut head = [0u8; 8];
        match sock.read_exact(&mut head).await { Ok(_) => {}, Err(_) => return Ok(()) }
        let data_len = u32::from_be_bytes([head[4],head[5],head[6],head[7]]) as usize;
        if data_len == 0 || data_len > 1_000_000 { return Ok(()); }
        let mut buf = vec![0u8; data_len + 4]; // data + CRC16(4)
        sock.read_exact(&mut buf).await?;
        let data = &buf[..data_len];
        match decode_avl(data) {
            Ok(records) => {
                let n = records.len() as u32;
                for r in &records {
                    let ts_rfc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(r.ts_ms)
                        .unwrap_or_else(chrono::Utc::now).to_rfc3339();
                    // allarme: event IO id mappato via config tenant teltonika.io_alarm_map
                    let alarm = alarm_for(&st, tid, r.event_io).await;
                    crate::api::teltonika_ingest_one(&st, tid, &imei, &source, &ts_rfc,
                        r.lat, r.lon, r.speed, r.course, r.battery, alarm).await;
                }
                sock.write_all(&n.to_be_bytes()).await?; // ack = num record accettati
            }
            Err(e) => { tracing::debug!("teltonika: decode errore ({e})"); sock.write_all(&0u32.to_be_bytes()).await?; }
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
pub struct Rec { pub ts_ms: i64, pub lat: f64, pub lon: f64, pub speed: Option<f64>, pub course: Option<f64>, pub battery: Option<f64>, pub event_io: i64 }

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
        let mut read_set = |c: &mut Cur, vlen: usize| -> Result<(),String> {
            let n = if ext { c.u16()? as usize } else { c.u8()? as usize };
            for _ in 0..n {
                let id = if ext { c.u16()? as i64 } else { c.u8()? as i64 };
                let raw = c.take(vlen)?;
                let mut val: i64 = 0; for &x in raw { val = (val << 8) | x as i64; }
                // IO 113 = livello batteria % (FMB/GH). Fallback: 68 battery current ignorato.
                if id == 113 { battery = Some(val as f64); }
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
        out.push(Rec{ ts_ms, lat, lon, speed: Some(speed), course: Some(angle), battery, event_io });
    }
    let count2 = c.u8()? as usize;
    if count2 != count { return Err(format!("mismatch record count {count} != {count2}")); }
    Ok(out)
}
