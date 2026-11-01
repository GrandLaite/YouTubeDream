//! Протокол Native Messaging. Каждое сообщение это JSON в UTF-8, перед ним
//! длина в 4 байта в порядке байтов платформы (на всех наших little-endian).

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

/// Больше 1 МБ от помощника браузер не принимает и рвёт соединение.
const MAX_OUT: usize = 1 << 20;
/// От браузера приходит не больше 64 МБ.
const MAX_IN: usize = 64 << 20;

/// Читает одно сообщение. `None` значит, что браузер закрыл соединение.
pub fn read_message(r: &mut impl Read) -> io::Result<Option<Value>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_ne_bytes(len) as usize;
    if len > MAX_IN {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("сообщение длиной {len} байт")));
    }
    let mut body = vec![0; len];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map(Some).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Сообщение в том виде, в каком оно уходит в браузер.
pub fn encode(msg: &Value) -> Vec<u8> {
    let mut body = serde_json::to_vec(msg).unwrap_or_default();
    if body.len() > MAX_OUT {
        // Слишком длинный ответ сломал бы соединение целиком, поэтому отдаём ошибку
        // вместо него. На практике так могла бы вырасти только ошибка yt-dlp.
        let reply = msg.get("reply").cloned().unwrap_or(Value::Null);
        body = serde_json::to_vec(&json!({ "reply": reply, "ok": false, "error": "ответ помощника слишком велик" }))
            .unwrap_or_default();
    }
    let mut out = Vec::with_capacity(body.len() + 4);
    out.extend_from_slice(&(body.len() as u32).to_ne_bytes());
    out.extend_from_slice(&body);
    out
}

/// Канал в браузер. Пишут в него несколько потоков (ответы, прогресс
/// загрузки, установка компонентов), поэтому сообщение уходит целиком под
/// мьютексом, иначе куски разных сообщений перемешались бы.
#[derive(Clone)]
pub struct Out {
    inner: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl Out {
    pub fn new(w: impl Write + Send + 'static) -> Self {
        Self { inner: Arc::new(Mutex::new(Box::new(w))) }
    }

    pub fn send(&self, msg: Value) {
        let data = encode(&msg);
        let mut w = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Ошибка записи значит, что браузер уже закрыл соединение: цикл
        // чтения вот-вот получит конец потока и завершит помощник.
        let _ = w.write_all(&data).and_then(|()| w.flush());
    }

    /// Событие, о котором браузер не спрашивал: прогресс, компоненты.
    pub fn event(&self, name: &str, mut body: Value) {
        if let Some(obj) = body.as_object_mut() {
            obj.insert("event".into(), name.into());
        }
        self.send(body);
    }

    pub fn reply(&self, id: &Value, result: Result<Value, Value>) {
        let msg = match result {
            Ok(data) => json!({ "reply": id, "ok": true, "data": data }),
            Err(mut err) => {
                if let Some(obj) = err.as_object_mut() {
                    obj.insert("reply".into(), id.clone());
                    obj.insert("ok".into(), false.into());
                }
                err
            }
        };
        self.send(msg);
    }
}

/// Ошибка для ответа: `{"error": "…"}`.
pub fn fail(text: impl Into<String>) -> Value {
    json!({ "error": text.into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let msg = json!({ "cmd": "hello", "text": "Привет" });
        let data = encode(&msg);
        assert_eq!(u32::from_ne_bytes(data[..4].try_into().unwrap()) as usize, data.len() - 4);
        let mut r = &data[..];
        assert_eq!(read_message(&mut r).unwrap(), Some(msg));
        assert_eq!(read_message(&mut r).unwrap(), None);
    }

    #[test]
    fn oversized_reply_becomes_error() {
        let huge = "x".repeat(MAX_OUT + 10);
        let data = encode(&json!({ "reply": 7, "ok": false, "error": huge }));
        let msg: Value = serde_json::from_slice(&data[4..]).unwrap();
        assert_eq!(msg["reply"], 7);
        assert_eq!(msg["ok"], false);
        assert!(data.len() < 1024);
    }

    #[test]
    fn replies_and_events() {
        #[derive(Clone, Default)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl Write for Sink {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let sink = Sink::default();
        let out = Out::new(sink.clone());
        out.reply(&json!(1), Ok(json!({ "a": 1 })));
        out.reply(&json!(2), Err(json!({ "error": "нет", "auth": true })));
        out.event("job", json!({ "id": "j" }));
        let bytes = sink.0.lock().unwrap().clone();
        let mut r = &bytes[..];
        let first = read_message(&mut r).unwrap().unwrap();
        assert_eq!(first, json!({ "reply": 1, "ok": true, "data": { "a": 1 } }));
        let second = read_message(&mut r).unwrap().unwrap();
        assert_eq!(second, json!({ "reply": 2, "ok": false, "error": "нет", "auth": true }));
        let third = read_message(&mut r).unwrap().unwrap();
        assert_eq!(third, json!({ "event": "job", "id": "j" }));
    }
}
