use std::io::{BufReader, Read, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::local_socket::{Reach, Stream};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case", tag = "order")]
pub enum Order {
    Kill,
    Wait,
    Follow { offset: u64 },
}

impl crate::ingress::Ingress for Order {}

pub fn read_order(stream: &Stream) -> std::io::Result<Order> {
    let mut reader = BufReader::new(stream);
    let line = crate::bounded::line(&mut reader, crate::bounded::CONTROL_LINE)?;
    crate::ingress::json(&line).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

#[derive(Debug)]
pub struct Session(Stream);

impl Session {
    pub fn open(path: &Path, order: Order) -> std::io::Result<Reach<Self>> {
        let stream = match Stream::connect(path)? {
            Reach::Reached(stream) => stream,
            Reach::NobodyListening => return Ok(Reach::NobodyListening),
        };
        let mut line = serde_json::to_vec(&order).map_err(std::io::Error::other)?;
        line.push(b'\n');
        (&stream).write_all(&line)?;
        stream.close_sending()?;
        Ok(Reach::Reached(Self(stream)))
    }

    pub fn relay(&self, sink: &mut dyn Write) -> std::io::Result<u64> {
        let mut forwarded = 0u64;
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            let read = (&self.0).read(&mut buffer)?;
            let Some(chunk) = buffer.get(..read).filter(|c| !c.is_empty()) else {
                sink.flush()?;
                return Ok(forwarded);
            };
            sink.write_all(chunk)?;
            sink.flush()?;
            forwarded = forwarded.saturating_add(crate::domain::len_u64(chunk.len()));
        }
    }

    pub fn abandon(&self) -> std::io::Result<()> {
        self.0.close_both()
    }
}
