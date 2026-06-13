/// Real-log property test (FIX 4).
///
/// For each adapter (Claude/Codex/pi), discovers up to 30 real sessions from the
/// actual data directories (~/.claude, ~/.codex, ~/.pi) IF they exist. For every
/// parsed record asserts the non-overlap invariant:
///   rec.input + rec.cache_write + rec.cache_read == rec.context_size
///
/// The whole test is guarded: if the adapter's home directory doesn't exist, the
/// test skips (returns early) so CI without real data still passes.
use tokenprinter::adapters::all_adapters;

const CAP: usize = 30;

#[test]
fn real_sessions_pass_context_size_invariant() {
    let mut total_validated = 0usize;

    for adapter in all_adapters() {
        let refs = match adapter.discover() {
            Ok(r) => r,
            // If discovery fails (dir missing, permission error), skip silently.
            Err(_) => continue,
        };
        if refs.is_empty() {
            // Data dir doesn't exist or has no sessions — skip this adapter.
            continue;
        }

        let capped = refs.iter().take(CAP);
        for sref in capped {
            let sd = match adapter.parse(sref) {
                Ok(s) => s,
                Err(_) => continue, // skip unparseable sessions gracefully
            };
            for rec in &sd.records {
                // context_size is set by adapters as input + cache_read (+ cache_write for
                // adapters that track it). Assert the adapter's own invariant holds.
                assert_eq!(
                    rec.input + rec.cache_write + rec.cache_read,
                    rec.context_size,
                    "context_size invariant violated in {} session '{}' model '{}': \
                     input({}) + cache_write({}) + cache_read({}) = {} != context_size({})",
                    adapter.agent().slug(), rec.session_id, rec.model,
                    rec.input, rec.cache_write, rec.cache_read,
                    rec.input + rec.cache_write + rec.cache_read,
                    rec.context_size,
                );
                total_validated += 1;
            }
        }
    }

    eprintln!("real_data: validated {} records across all adapters (cap {} sessions each)", total_validated, CAP);
}
