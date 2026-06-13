// agy.rs — Antigravity (agy) adapter for tokenprinter
//
// DECISION (from STEP 0 investigation):
//   Antigravity stores per-session data in SQLite .db files under
//   ~/.gemini/antigravity-cli/conversations/<uuid>.db.
//   Each .db has a `gen_metadata` table where each row is a per-turn
//   protobuf blob containing:
//     - F4 (UUID): step/turn identifier
//     - F1 (nested message): session context, containing:
//         F4 (nested): token counts sub-message with
//           F2 = input/prompt tokens for this turn
//           F3 = output/response tokens for this turn
//           F5 = context window used (when present)
//   The human-readable model name (e.g. "Gemini 3.5 Flash (High)") is
//   present as an ASCII string in the blob.
//
//   history.jsonl contains only user-prompt display strings (no token data).
//   The .pb conversation files are binary protobuf but contain no token counts.
//   Only the SQLite .db files have per-turn token counts — we parse those.
//
//   Token mapping (no cache breakdown available from agy):
//     input      = F1.F4.F2 (prompt tokens per turn)
//     output     = F1.F4.F3 (response tokens per turn)
//     cache_write = 0
//     cache_read  = 0
//     context_size = input + cache_write + cache_read = input
//   Each turn becomes one UsageRecord (non-overlapping, per-turn, not cumulative).
//
//   If no token data is found in a file, we emit a one-line stderr note and
//   return an empty records list. We never panic or block.

use super::{Adapter, SessionRef};
use crate::model::{Agent, CacheTtl, SessionData, UsageRecord};
use chrono::{DateTime, TimeZone, Utc};
use std::path::PathBuf;

pub struct AgyAdapter {
    root: PathBuf,
}

impl AgyAdapter {
    pub fn new() -> Self {
        let root = dirs::home_dir()
            .unwrap_or_default()
            .join(".gemini/antigravity-cli/conversations");
        Self { root }
    }
}

// ── Protobuf varint decoder ──────────────────────────────────────────────────

/// Decode a protobuf varint from `data` starting at position `i`.
/// Returns `(value, new_i)` or `None` if the data is truncated/malformed.
fn decode_varint(data: &[u8], mut i: usize) -> Option<(u64, usize)> {
    let mut val: u64 = 0;
    let mut shift = 0u32;
    loop {
        if i >= data.len() {
            return None;
        }
        let b = data[i];
        i += 1;
        val |= ((b & 0x7F) as u64) << shift;
        shift += 7;
        if b & 0x80 == 0 {
            return Some((val, i));
        }
        if shift >= 64 {
            return None; // overflow guard
        }
    }
}

/// Skip a single protobuf field value (without reading it), returning new position.
fn skip_field(data: &[u8], i: usize, wire_type: u8) -> Option<usize> {
    match wire_type {
        0 => {
            // varint
            let (_, ni) = decode_varint(data, i)?;
            Some(ni)
        }
        1 => {
            // 64-bit
            if i + 8 <= data.len() {
                Some(i + 8)
            } else {
                None
            }
        }
        2 => {
            // length-delimited
            let (len, ni) = decode_varint(data, i)?;
            let end = ni + len as usize;
            if end <= data.len() {
                Some(end)
            } else {
                None
            }
        }
        5 => {
            // 32-bit
            if i + 4 <= data.len() {
                Some(i + 4)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Parsed token counts from one gen_metadata row.
#[derive(Debug, Default)]
struct TurnTokens {
    input: u64,
    output: u64,
}

/// Parse the token count sub-message (F4 inside F1 of a gen_metadata blob).
/// Returns (f2, f3, f5) = (input_tokens, output_tokens, ctx_window) where 0 means absent.
fn parse_f4_submessage(data: &[u8]) -> (u64, u64, u64) {
    let mut f2: u64 = 0;
    let mut f3: u64 = 0;
    let mut f5: u64 = 0;
    let mut i = 0;
    while i < data.len() {
        let (tag, ni) = match decode_varint(data, i) {
            Some(v) => v,
            None => break,
        };
        i = ni;
        let field_num = (tag >> 3) as u32;
        let wire_type = (tag & 0x7) as u8;
        match (field_num, wire_type) {
            (2, 0) => {
                if let Some((v, ni)) = decode_varint(data, i) {
                    f2 = v;
                    i = ni;
                } else {
                    break;
                }
            }
            (3, 0) => {
                if let Some((v, ni)) = decode_varint(data, i) {
                    f3 = v;
                    i = ni;
                } else {
                    break;
                }
            }
            (5, 0) => {
                if let Some((v, ni)) = decode_varint(data, i) {
                    f5 = v;
                    i = ni;
                } else {
                    break;
                }
            }
            _ => {
                i = match skip_field(data, i, wire_type) {
                    Some(ni) => ni,
                    None => break,
                };
            }
        }
    }
    (f2, f3, f5)
}

/// Parse one gen_metadata blob, extracting per-turn token counts from field F1 > F4.
/// Returns None if no usable data found.
fn parse_gen_metadata_blob(blob: &[u8]) -> Option<TurnTokens> {
    let mut i = 0;
    let mut turn = TurnTokens::default();
    let mut found = false;

    while i < blob.len() {
        let (tag, ni) = decode_varint(blob, i)?;
        i = ni;
        let field_num = (tag >> 3) as u32;
        let wire_type = (tag & 0x7) as u8;

        if wire_type == 2 {
            let (len, ni) = decode_varint(blob, i)?;
            let end = ni + len as usize;
            if end > blob.len() {
                break;
            }
            let content = &blob[ni..end];

            if field_num == 1 {
                // Parse the nested F1 message to find F4 (token counts sub-message)
                let mut j = 0;
                while j < content.len() {
                    let (ctag, nj) = match decode_varint(content, j) {
                        Some(v) => v,
                        None => break,
                    };
                    j = nj;
                    let cfn = (ctag >> 3) as u32;
                    let cwt = (ctag & 0x7) as u8;

                    if cwt == 2 {
                        let (clen, nj) = match decode_varint(content, j) {
                            Some(v) => v,
                            None => break,
                        };
                        let cend = nj + clen as usize;
                        if cend > content.len() {
                            break;
                        }
                        let sub = &content[nj..cend];

                        if cfn == 4 {
                            let (f2, f3, _f5) = parse_f4_submessage(sub);
                            if f2 > 0 || f3 > 0 {
                                turn.input = f2;
                                turn.output = f3;
                                found = true;
                            }
                        }
                        j = cend;
                    } else {
                        j = match skip_field(content, j, cwt) {
                            Some(nj) => nj,
                            None => break,
                        };
                    }
                }
                i = end;
            } else {
                i = end;
            }
        } else {
            i = match skip_field(blob, i, wire_type) {
                Some(ni) => ni,
                None => break,
            };
        }
    }

    if found {
        Some(turn)
    } else {
        None
    }
}

/// Extract the model display name from a gen_metadata blob by scanning for
/// the "Gemini " prefix in the printable ASCII bytes.
fn extract_model_name(blob: &[u8]) -> Option<String> {
    // Find "Gemini " as a byte pattern in printable content
    let needle = b"Gemini ";
    let pos = blob.windows(needle.len()).position(|w| w == needle)?;
    // Collect printable ASCII bytes after the match
    let s: String = blob[pos..]
        .iter()
        .copied()
        .take_while(|&b| b >= 0x20 && b < 0x7F)
        .map(|b| b as char)
        .collect();
    if s.len() > 7 {
        Some(s.trim().to_string())
    } else {
        None
    }
}

/// Extract timestamp from a gen_metadata blob.
/// The step creation time is embedded in F1 > F9 > F4 (a Timestamp sub-message).
/// We do a best-effort scan: look for the F9 field inside F1, then parse F4
/// inside it as seconds (F1) + nanos (F2).
fn extract_timestamp_from_blob(blob: &[u8]) -> Option<DateTime<Utc>> {
    let mut i = 0;
    while i < blob.len() {
        let (tag, ni) = decode_varint(blob, i)?;
        i = ni;
        let field_num = (tag >> 3) as u32;
        let wire_type = (tag & 0x7) as u8;

        if wire_type == 2 {
            let (len, ni) = decode_varint(blob, i)?;
            let end = ni + len as usize;
            if end > blob.len() {
                break;
            }
            let content = &blob[ni..end];

            if field_num == 1 {
                // Inside F1, look for F9 (timer context)
                let mut j = 0;
                while j < content.len() {
                    let (ctag, nj) = match decode_varint(content, j) {
                        Some(v) => v,
                        None => break,
                    };
                    j = nj;
                    let cfn = (ctag >> 3) as u32;
                    let cwt = (ctag & 0x7) as u8;

                    if cwt == 2 {
                        let (clen, nj) = match decode_varint(content, j) {
                            Some(v) => v,
                            None => break,
                        };
                        let cend = nj + clen as usize;
                        if cend > content.len() {
                            break;
                        }
                        let sub = &content[nj..cend];

                        if cfn == 9 {
                            // Parse F9 for timestamp (F4 > F1=secs, F2=nanos)
                            if let Some(ts) = parse_timestamp_submessage(sub) {
                                return Some(ts);
                            }
                        }
                        j = cend;
                    } else {
                        j = match skip_field(content, j, cwt) {
                            Some(nj) => nj,
                            None => break,
                        };
                    }
                }
                i = end;
            } else {
                i = end;
            }
        } else {
            i = match skip_field(blob, i, wire_type) {
                Some(ni) => ni,
                None => break,
            };
        }
    }
    None
}

/// Parse the F9 sub-message looking for F4 which is a Timestamp (F1=secs, F2=nanos).
fn parse_timestamp_submessage(data: &[u8]) -> Option<DateTime<Utc>> {
    let mut i = 0;
    while i < data.len() {
        let (tag, ni) = decode_varint(data, i)?;
        i = ni;
        let field_num = (tag >> 3) as u32;
        let wire_type = (tag & 0x7) as u8;

        if field_num == 4 && wire_type == 2 {
            let (len, ni) = decode_varint(data, i)?;
            let end = ni + len as usize;
            if end > data.len() {
                return None;
            }
            let sub = &data[ni..end];
            // Parse F1=secs, F2=nanos
            let mut secs: i64 = 0;
            let mut j = 0;
            while j < sub.len() {
                let (stag, nj) = match decode_varint(sub, j) {
                    Some(v) => v,
                    None => break,
                };
                j = nj;
                let sfn = (stag >> 3) as u32;
                let swt = (stag & 0x7) as u8;
                if sfn == 1 && swt == 0 {
                    if let Some((v, nj)) = decode_varint(sub, j) {
                        secs = v as i64;
                        j = nj;
                    } else {
                        break;
                    }
                } else {
                    j = match skip_field(sub, j, swt) {
                        Some(nj) => nj,
                        None => break,
                    };
                }
            }
            if secs > 0 {
                return Utc.timestamp_opt(secs, 0).single();
            }
            i = end;
        } else {
            i = match skip_field(data, i, wire_type) {
                Some(ni) => ni,
                None => break,
            };
        }
    }
    None
}

// ── SQLite open helper ────────────────────────────────────────────────────────

/// Open a SQLite database read-only. Tries two strategies:
/// 1. Plain READONLY + NO_MUTEX flags (works for files with WAL companions).
/// 2. URI with `immutable=1` (works for files without WAL companions that rusqlite
///    can't open in the first mode).
fn open_db_readonly(path: &std::path::Path) -> anyhow::Result<Option<rusqlite::Connection>> {
    // Strategy 1: direct read-only.
    let flags_ro = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    if let Ok(conn) = rusqlite::Connection::open_with_flags(path, flags_ro) {
        return Ok(Some(conn));
    }
    // Strategy 2: URI immutable (avoids needing shm/wal companions).
    let uri = format!(
        "file:{}?mode=ro&immutable=1",
        path.to_string_lossy()
    );
    let flags_uri = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
        | rusqlite::OpenFlags::SQLITE_OPEN_URI
        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    match rusqlite::Connection::open_with_flags(&uri, flags_uri) {
        Ok(conn) => Ok(Some(conn)),
        Err(e) => anyhow::bail!("{e}"),
    }
}

// ── Adapter impl ─────────────────────────────────────────────────────────────

impl Adapter for AgyAdapter {
    fn agent(&self) -> Agent {
        Agent::Agy
    }

    fn discover(&self) -> anyhow::Result<Vec<SessionRef>> {
        let mut out = Vec::new();
        if !self.root.exists() {
            return Ok(out);
        }
        let rd = match std::fs::read_dir(&self.root) {
            Ok(rd) => rd,
            Err(_) => return Ok(out),
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().map(|e| e == "db").unwrap_or(false) {
                let sid = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                if sid.is_empty() {
                    continue;
                }
                out.push(SessionRef {
                    agent: Agent::Agy,
                    session_id: sid,
                    path: p,
                });
            }
        }
        Ok(out)
    }

    fn parse(&self, r: &SessionRef) -> anyhow::Result<SessionData> {
        // Try read-only first; fall back to URI immutable mode for WAL-less databases.
        let conn = open_db_readonly(&r.path).unwrap_or_else(|e| {
            eprintln!("agy: cannot open {}: {e}", r.path.display());
            None
        });
        let conn = match conn {
            Some(c) => c,
            None => return Ok(empty_session(r)),
        };

        // Read all gen_metadata rows ordered by idx.
        let blobs: Vec<Vec<u8>> = {
            let mut stmt = match conn
                .prepare("SELECT data FROM gen_metadata ORDER BY idx ASC")
            {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "agy: cannot query gen_metadata in {}: {e}",
                        r.path.display()
                    );
                    return Ok(empty_session(r));
                }
            };
            let rows = stmt.query_map([], |row| {
                row.get::<_, Vec<u8>>(0)
            });
            match rows {
                Ok(rows) => rows.filter_map(|r| r.ok()).collect(),
                Err(e) => {
                    eprintln!(
                        "agy: error reading gen_metadata in {}: {e}",
                        r.path.display()
                    );
                    return Ok(empty_session(r));
                }
            }
        };

        if blobs.is_empty() {
            eprintln!("agy: no token usage found in {}", r.path.display());
            return Ok(empty_session(r));
        }

        // Extract model name from the first blob that has one.
        let model: String = blobs
            .iter()
            .find_map(|b| extract_model_name(b))
            .unwrap_or_else(|| "gemini-unknown".to_string());

        let mut records: Vec<UsageRecord> = Vec::new();
        let mut first_ts: Option<DateTime<Utc>> = None;
        let mut last_ts: Option<DateTime<Utc>> = None;

        for blob in &blobs {
            // Best-effort: skip blobs we can't parse.
            let turn = match parse_gen_metadata_blob(blob) {
                Some(t) => t,
                None => continue,
            };
            // Skip turns with no token data (e.g., tool steps).
            if turn.input == 0 && turn.output == 0 {
                continue;
            }

            let ts = extract_timestamp_from_blob(blob)
                .unwrap_or_else(Utc::now);

            if first_ts.is_none() || Some(ts) < first_ts {
                first_ts = Some(ts);
            }
            if last_ts.is_none() || Some(ts) > last_ts {
                last_ts = Some(ts);
            }

            // No cache breakdown available in agy/Antigravity data.
            // input = prompt tokens, output = response tokens.
            // context_size = input + cache_write + cache_read = input + 0 + 0.
            let input = turn.input;
            let output = turn.output;
            records.push(UsageRecord {
                agent: Agent::Agy,
                provider: "google".into(),
                model: model.clone(),
                session_id: r.session_id.clone(),
                project: None,
                timestamp: ts,
                input,
                output,
                cache_write: 0,
                cache_read: 0,
                reasoning: 0,
                context_size: input,  // = input + 0 + 0
                cache_write_ttl: CacheTtl::FiveMin,
                cost: None,
            });
        }

        if records.is_empty() {
            eprintln!("agy: no token usage found in {}", r.path.display());
        }

        let started = first_ts.unwrap_or_else(Utc::now);
        let ended = last_ts.unwrap_or(started);
        let turns = records.len() as u32;

        Ok(SessionData {
            agent: Agent::Agy,
            session_id: r.session_id.clone(),
            project: None,
            git_branch: None,
            started_at: started,
            ended_at: ended,
            records,
            tool_calls: Vec::new(),
            turns,
        })
    }
}

fn empty_session(r: &SessionRef) -> SessionData {
    SessionData {
        agent: Agent::Agy,
        session_id: r.session_id.clone(),
        project: None,
        git_branch: None,
        started_at: Utc::now(),
        ended_at: Utc::now(),
        records: Vec::new(),
        tool_calls: Vec::new(),
        turns: 0,
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{Adapter, SessionRef};
    use crate::model::Agent;
    use std::path::PathBuf;

    #[test]
    fn parse_agy_session_extracts_nonzero_tokens() {
        let a = AgyAdapter::new();
        let r = SessionRef {
            agent: Agent::Agy,
            session_id: "agy-test".into(),
            path: PathBuf::from("tests/fixtures/agy_session.db"),
        };
        let s = a.parse(&r).unwrap();
        assert_eq!(s.agent, Agent::Agy);
        // Fixture has 2 turns with known token counts.
        assert_eq!(s.records.len(), 2, "expected 2 records from fixture");
        let rec = &s.records[0];
        // Turn 1: input=1234, output=567
        assert_eq!(rec.input, 1234);
        assert_eq!(rec.output, 567);
        assert_eq!(rec.cache_write, 0);
        assert_eq!(rec.cache_read, 0);
        // Invariant: context_size == input + cache_write + cache_read
        assert_eq!(rec.context_size, rec.input + rec.cache_write + rec.cache_read);
        assert_eq!(rec.provider, "google");
    }

    #[test]
    fn parse_agy_nonzero_tokens_invariant_all_records() {
        let a = AgyAdapter::new();
        let r = SessionRef {
            agent: Agent::Agy,
            session_id: "agy-test".into(),
            path: PathBuf::from("tests/fixtures/agy_session.db"),
        };
        let s = a.parse(&r).unwrap();
        for rec in &s.records {
            assert_eq!(
                rec.input + rec.cache_write + rec.cache_read,
                rec.context_size,
                "context_size invariant violated for record model={}", rec.model
            );
        }
    }

    #[test]
    fn parse_garbage_bytes_returns_empty_no_panic() {
        let a = AgyAdapter::new();
        let r = SessionRef {
            agent: Agent::Agy,
            session_id: "agy-garbage".into(),
            path: PathBuf::from("tests/fixtures/agy_garbage.db"),
        };
        // Must not panic; returns empty records gracefully.
        let s = a.parse(&r).unwrap();
        assert_eq!(s.agent, Agent::Agy);
        assert!(
            s.records.is_empty(),
            "garbage db should produce no records"
        );
    }

    #[test]
    fn discover_returns_empty_when_dir_missing() {
        let adapter = AgyAdapter {
            root: PathBuf::from("/tmp/nonexistent_agy_dir_xyz_12345"),
        };
        let refs = adapter.discover().unwrap();
        assert!(refs.is_empty(), "discover() must return empty when dir is missing");
    }

    #[test]
    fn parse_protobuf_varint_roundtrip() {
        // encode a few values as varints and verify decode
        let cases: &[(u64, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01]),
            (127, &[0x7F]),
            (128, &[0x80, 0x01]),
            (300, &[0xAC, 0x02]),
            (1234, &[0xD2, 0x09]),
            (567, &[0xB7, 0x04]),
        ];
        for (expected, bytes) in cases {
            let (val, _) = decode_varint(bytes, 0).unwrap();
            assert_eq!(val, *expected, "varint mismatch for expected={}", expected);
        }
    }
}
