//! One client connection: request framing, command lists and `idle`.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info};

use super::commands::Session;
use super::parse::tokenize;
use super::{Ack, AckCode, Ctx, Out, PROTOCOL_VERSION};
use crate::events::Subsystem;

/// Longest request line accepted, to bound memory per client.
const MAX_LINE: usize = 1 << 20;

pub async fn handle<S: AsyncRead + AsyncWrite + Unpin>(stream: S, ctx: Arc<Ctx>, peer: String) {
    debug!("{peer}: connected");
    if let Err(e) = run(stream, ctx).await {
        debug!("{peer}: {e}");
    }
    debug!("{peer}: disconnected");
}

enum Read {
    Line(String),
    Eof,
}

struct Conn<R> {
    reader: BufReader<R>,
    /// Survives a cancelled read inside `idle`.
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> Conn<R> {
    async fn read_line(&mut self) -> std::io::Result<Read> {
        loop {
            let n = AsyncReadExt::take(&mut self.reader, (MAX_LINE + 1 - self.buf.len()) as u64)
                .read_until(b'\n', &mut self.buf)
                .await?;
            if self.buf.last() == Some(&b'\n') {
                break;
            }
            if n == 0 {
                return Ok(Read::Eof);
            }
            if self.buf.len() > MAX_LINE {
                return Err(std::io::Error::other("request line too long"));
            }
        }
        let line = String::from_utf8_lossy(&self.buf).trim_end_matches(['\r', '\n']).to_owned();
        self.buf.clear();
        Ok(Read::Line(line))
    }
}

async fn run<S: AsyncRead + AsyncWrite + Unpin>(stream: S, ctx: Arc<Ctx>) -> std::io::Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut conn = Conn { reader: BufReader::new(reader), buf: Vec::new() };
    let mut events = ctx.events.subscribe();
    let mut pending: HashSet<Subsystem> = HashSet::new();
    let mut session = Session::new(ctx);

    writer.write_all(format!("OK MPD {PROTOCOL_VERSION}\n").as_bytes()).await?;

    loop {
        let Read::Line(line) = conn.read_line().await? else { return Ok(()) };
        let (name, args) = match tokenize(&line) {
            Ok(parsed) => parsed,
            Err(e) => {
                writer.write_all(Ack::arg(e).render(0, "").as_bytes()).await?;
                continue;
            }
        };
        debug!("> {line}");

        match name.as_str() {
            "close" => return Ok(()),
            // Outside idle, MPD ignores noidle without a response.
            "noidle" => {}
            "idle" => {
                drain(&mut events, &mut pending);
                let mask = match idle_mask(&args) {
                    Ok(mask) => mask,
                    Err(ack) => {
                        writer.write_all(ack.render(0, "idle").as_bytes()).await?;
                        continue;
                    }
                };
                let mut fired: Vec<Subsystem> = pending.iter().copied().filter(|s| mask.contains(s)).collect();
                while fired.is_empty() {
                    tokio::select! {
                        event = events.recv() => {
                            match event {
                                Ok(s) => { pending.insert(s); }
                                Err(RecvError::Lagged(_)) => pending.extend(Subsystem::ALL),
                                Err(RecvError::Closed) => return Ok(()),
                            }
                            // Report events emitted together in one response.
                            tokio::task::yield_now().await;
                            drain(&mut events, &mut pending);
                            fired = pending.iter().copied().filter(|s| mask.contains(s)).collect();
                        }
                        read = conn.read_line() => {
                            match read? {
                                Read::Line(l) if l.trim() == "noidle" => break,
                                // Any other command during idle is a protocol error.
                                _ => return Ok(()),
                            }
                        }
                    }
                }
                let mut out = Out::default();
                for s in &fired {
                    pending.remove(s);
                    out.push_str(&format!("changed: {}\n", s.name()));
                }
                out.push_str("OK\n");
                writer.write_all(out.as_bytes()).await?;
            }
            "command_list_begin" | "command_list_ok_begin" => {
                let list_ok = name == "command_list_ok_begin";
                let mut lines = Vec::new();
                loop {
                    match conn.read_line().await? {
                        Read::Line(l) if l == "command_list_end" => break,
                        Read::Line(l) => lines.push(l),
                        Read::Eof => return Ok(()),
                    }
                }
                let mut out = Out::default();
                let mut failed = false;
                for (index, l) in lines.iter().enumerate() {
                    debug!("> [{index}] {l}");
                    let result = match tokenize(l) {
                        Ok((name, args)) => run_one(&mut session, &name, &args, &mut out).await.map_err(|a| (a, name)),
                        Err(e) => Err((Ack::arg(e), String::new())),
                    };
                    match result {
                        Ok(()) if list_ok => out.push_str("list_OK\n"),
                        Ok(()) => {}
                        Err((ack, name)) => {
                            out.push_str(&ack.render(index, &name));
                            failed = true;
                            break;
                        }
                    }
                }
                if !failed {
                    out.push_str("OK\n");
                }
                writer.write_all(out.as_bytes()).await?;
            }
            _ => {
                let mut out = Out::default();
                match run_one(&mut session, &name, &args, &mut out).await {
                    Ok(()) => out.push_str("OK\n"),
                    Err(ack) => {
                        out.clear();
                        out.push_str(&ack.render(0, &name));
                    }
                }
                writer.write_all(out.as_bytes()).await?;
            }
        }
    }
}

async fn run_one(session: &mut Session, name: &str, args: &[String], out: &mut Out) -> Result<(), Ack> {
    match name {
        "idle" | "noidle" | "close" | "command_list_begin" | "command_list_ok_begin" | "command_list_end" => {
            Err(Ack::new(AckCode::Arg, format!("{name} not allowed in command list")))
        }
        _ => {
            let result = session.exec(name, args, out).await;
            if let Err(ack) = &result
                && ack.code == AckCode::Unknown
            {
                info!("unsupported command: {name} {args:?}");
            }
            result
        }
    }
}

fn drain(events: &mut tokio::sync::broadcast::Receiver<Subsystem>, pending: &mut HashSet<Subsystem>) {
    loop {
        match events.try_recv() {
            Ok(s) => {
                pending.insert(s);
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => pending.extend(Subsystem::ALL),
            Err(_) => break,
        }
    }
}

fn idle_mask(args: &[String]) -> Result<HashSet<Subsystem>, Ack> {
    if args.is_empty() {
        return Ok(Subsystem::ALL.into_iter().collect());
    }
    args.iter()
        .map(|a| Subsystem::from_name(a).ok_or_else(|| Ack::arg(format!("Unrecognized idle event: {a}"))))
        .collect()
}
