use crate::model::*;

const W: usize = 48;

/// Build a Star Line Mode raster bit-image for `data` encoded as a QR code.
///
/// Command layout emitted:
///   ESC GS S  = [0x1b, 0x1d, 0x53]
///   m         = 0x01  (raster mode)
///   xL, xH   = number of BYTES per dot-row (ceil(width_dots / 8)), little-endian 16-bit
///   yL, yH   = number of dot-rows (height_dots), little-endian 16-bit
///   <data>    = bitmap bytes, row-major, MSB = leftmost dot, 1 = black module
///
/// Each QR module is rendered as an N×N block of dots (N=4) for scannability.
/// Returns an empty Vec if encoding fails (e.g. data too large for QR).
pub fn qr_raster(data: &str) -> Vec<u8> {
    let code = match qrcode::QrCode::new(data.as_bytes()) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    const N: usize = 4; // dot-block size per QR module
    let modules = code.width();
    let dot_size = modules * N;

    // bytes_per_row: ceil(dot_size / 8)
    let bytes_per_row = dot_size.div_ceil(8);
    let height_dots = dot_size;

    // Build the bitmap: row-major, MSB = leftmost dot, 1 = black.
    let colors = code.to_colors();
    let mut bitmap: Vec<u8> = Vec::with_capacity(bytes_per_row * height_dots);

    for dot_row in 0..height_dots {
        let module_row = dot_row / N;
        let mut row_bytes = vec![0u8; bytes_per_row];
        for dot_col in 0..dot_size {
            let module_col = dot_col / N;
            let module_idx = module_row * modules + module_col;
            let is_dark = colors[module_idx] == qrcode::Color::Dark;
            if is_dark {
                let byte_idx = dot_col / 8;
                let bit = 7 - (dot_col % 8);
                row_bytes[byte_idx] |= 1 << bit;
            }
        }
        bitmap.extend_from_slice(&row_bytes);
    }

    // Assemble the Star Line raster command.
    let x_l = (bytes_per_row & 0xFF) as u8;
    let x_h = ((bytes_per_row >> 8) & 0xFF) as u8;
    let y_l = (height_dots & 0xFF) as u8;
    let y_h = ((height_dots >> 8) & 0xFF) as u8;

    let mut out: Vec<u8> = Vec::with_capacity(7 + bitmap.len());
    out.extend_from_slice(&[0x1b, 0x1d, 0x53]); // ESC GS S
    out.push(0x01);                               // mode = 1 (raster)
    out.push(x_l);
    out.push(x_h);
    out.push(y_l);
    out.push(y_h);
    out.extend_from_slice(&bitmap);
    out
}

/// Same as `render_bytes` but inserts a QR raster block after the text body and before
/// the final feed+cut when `qr_data` is `Some(&non_empty_str)` and `qr_raster` succeeds.
pub fn render_bytes_with_qr(r: &Receipt, qr_data: Option<&str>) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&[0x1b, 0x40]); // ESC @ init
    b.extend_from_slice(render_text(r).as_bytes());
    b.extend_from_slice(b"\n\n\n");

    if let Some(data) = qr_data {
        if !data.is_empty() {
            let raster = qr_raster(data);
            if !raster.is_empty() {
                b.extend_from_slice(&raster);
                b.extend_from_slice(b"\n\n"); // small feed after image
            }
        }
    }

    b.extend_from_slice(&[0x1b, 0x64, 0x02]); // ESC d 2 full cut
    b
}

fn rule(c: char) -> String { std::iter::repeat_n(c, W).collect() }
fn center(s: &str) -> String {
    let n = s.chars().count();
    if n >= W { return s.chars().take(W).collect(); }
    let pad = (W - n) / 2;
    format!("{}{}", " ".repeat(pad), s)
}
/// left text + right-aligned value on one W-wide line.
fn lr(left: &str, right: &str) -> String {
    let l = left.chars().count();
    let r = right.chars().count();
    if l + r >= W {
        // Hard-cap at W: keep as much of left as fits with a space and the right portion.
        let right_chars: Vec<char> = right.chars().collect();
        let r_take = right_chars.len().min(W.saturating_sub(1));
        let l_take = W.saturating_sub(r_take + 1);
        let left_trunc: String = left.chars().take(l_take).collect();
        let right_trunc: String = right_chars[..r_take].iter().collect();
        return format!("{left_trunc} {right_trunc}");
    }
    format!("{}{}{}", left, " ".repeat(W - l - r), right)
}
fn commafy(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 { out.push(','); }
        out.push(ch);
    }
    out.chars().rev().collect()
}
fn money(v: f64) -> String { format!("${v:.2}") }
fn dur(secs: i64) -> String {
    let h = secs / 3600; let m = (secs % 3600) / 60; let s = secs % 60;
    format!("{h:01}h {m:02}m {s:02}s")
}
const SPARK: [char; 8] = ['▁','▁','▂','▃','▅','▆','▇','█'];

pub fn render_text(r: &Receipt) -> String {
    let mut o = String::new();
    let push = |o: &mut String, l: String| { o.push_str(&l); o.push('\n'); };

    push(&mut o, center(&r.agent.provider().to_uppercase()));
    push(&mut o, rule('='));
    push(&mut o, center("TOKEN PRINTER"));
    if r.precompact {
        push(&mut o, center("PRE-COMPACTION MEMORIAL"));
    }
    push(&mut o, rule('='));
    push(&mut o, lr(" Agent", &format!("{} ", r.agent.label())));
    push(&mut o, lr(" Location", &format!("{} ", r.location)));
    push(&mut o, lr(" Session", &format!("{} ", trunc(&r.session_name, 32))));
    if let Some(p) = &r.project {
        let pb = project_value(p, r.git_branch.as_deref());
        push(&mut o, lr(" Project", &pb));
    }
    push(&mut o, lr(" Date", &format!("{} ", r.when.format("%Y-%m-%d %H:%M:%S"))));
    push(&mut o, lr(" Duration", &format!("{} ", dur(r.duration_secs))));

    push(&mut o, rule('-'));
    push(&mut o, " MODEL BREAKDOWN".into());
    push(&mut o, rule('-'));
    for m in &r.per_model {
        push(&mut o, format!(" {}", trunc(&m.model, 46)));
        push(&mut o, lr("   Input tokens", &format!("{} ", commafy(m.input))));
        push(&mut o, lr("   Output tokens", &format!("{} ", commafy(m.output))));
        push(&mut o, lr("   Cache write", &format!("{} ", commafy(m.cache_write))));
        push(&mut o, lr("   Cache read", &format!("{} ", commafy(m.cache_read))));
        let sub = match m.cost { Some(c)=>money(c), None=>"—".into() };
        push(&mut o, lr("   Subtotal", &format!("{sub} ")));
        push(&mut o, String::new());
    }

    let calls: u32 = r.tools.iter().map(|(_,c)| *c).sum();
    push(&mut o, rule('-'));
    push(&mut o, lr(" TOOL ACTIVITY", &format!("({calls} calls) ")));
    push(&mut o, rule('-'));
    let maxc = r.tools.iter().map(|(_,c)| *c).max().unwrap_or(1).max(1);
    for (name, c) in r.tools.iter().take(6) {
        let bars = ((*c as f64 / maxc as f64) * 11.0).round() as usize;
        let bar: String = std::iter::repeat_n('█', bars).collect();
        push(&mut o, lr(&format!("   {:<10}{}", trunc(name,10), bar), &format!("{c} ")));
    }
    if r.tools.len() > 6 {
        let rest: u32 = r.tools.iter().skip(6).map(|(_,c)| *c).sum();
        push(&mut o, lr(&format!("   +{} more", r.tools.len()-6), &format!("{rest} ")));
    }

    push(&mut o, rule('-'));
    push(&mut o, " PRODUCTIVITY".into());
    push(&mut o, rule('-'));
    push(&mut o, lr("   Files changed", &format!("{} ", r.git.files_changed)));
    push(&mut o, lr("   Lines", &format!("+{} / -{} ", r.git.added, r.git.removed)));
    push(&mut o, lr("   Commits", &format!("{} ", r.git.commits)));
    if !r.beads.opened.is_empty() {
        push(&mut o, lr("   Beads opened", &format!("{} ", trunc(&r.beads.opened.join(", "), 30))));
    }
    if !r.beads.closed.is_empty() {
        push(&mut o, lr("   Beads closed", &format!("{} ", trunc(&r.beads.closed.join(", "), 30))));
    }

    if !r.sparkline.is_empty() {
        push(&mut o, rule('-'));
        push(&mut o, " TOKENS OVER TIME".into());
        let spark: String = r.sparkline.iter().map(|&h| SPARK[(h as usize).min(7)]).collect();
        push(&mut o, format!("   {spark}"));
    }

    push(&mut o, rule('='));
    push(&mut o, lr(" SUBTOTAL", &format!("{} ", r.total_cost.map(money).unwrap_or("—".into()))));
    if let Some(s) = r.cache_saved_usd {
        push(&mut o, lr(" Cache savings", &format!("-{} ", money(s))));
    }
    push(&mut o, lr(" Sales tax (vibes, 0%)", "$0.00 "));
    push(&mut o, rule('='));
    push(&mut o, lr(" TOTAL", &format!("{} ", r.total_cost.map(money).unwrap_or("—".into()))));
    if let Some(note) = &r.billing_note {
        // Truncate to ≤48 cols so the line never exceeds receipt width.
        let note_trunc: String = note.chars().take(W).collect();
        push(&mut o, center(&note_trunc));
    }
    push(&mut o, rule('='));
    push(&mut o, lr(&format!(" Tokens: {}", commafy(r.total_tokens)),
        &match r.burn_rate_per_hr { Some(b)=>format!("Burn: {}/hr ", money(b)), None=>String::from("") }));
    push(&mut o, format!(" Cache hit rate: {:.1}%", r.cache_hit_rate));
    push(&mut o, String::new());
    push(&mut o, center("Thank you for vibe coding!"));
    push(&mut o, center("*** NO REFUNDS ON TOKENS ***"));
    push(&mut o, rule('='));
    o
}

/// Build the right-hand value for the Project line such that `lr(" Project", value)` fits W cols.
/// Keeps the tail of the path and prefixes with `…` when truncated.
fn project_value(raw_path: &str, branch: Option<&str>) -> String {
    let path = short_path(raw_path);
    // " Project" = 8 chars, 1 space separator, value chars + trailing space
    // lr produces W-wide only when l+r < W; safe budget for the right value:
    let label_len = " Project".chars().count(); // 8
    // branch suffix: " (branch) " including trailing space
    let branch_suffix_len = branch.map(|b| 3 + b.chars().count() + 2).unwrap_or(1); // " (" + b + ") " or just " "
    // budget for path chars = W - label_len - 1(space) - branch_suffix_len
    let path_budget = W.saturating_sub(label_len + 1 + branch_suffix_len);
    let path_chars: Vec<char> = path.chars().collect();
    let truncated_path = if path_chars.len() <= path_budget {
        path.clone()
    } else {
        let tail: String = path_chars[path_chars.len().saturating_sub(path_budget.saturating_sub(1))..].iter().collect();
        format!("…{tail}")
    };
    match branch {
        Some(b) => format!("{truncated_path} ({b}) "),
        None => format!("{truncated_path} "),
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n.saturating_sub(1)).collect::<String>() + "…" }
}
fn short_path(p: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        if let Some(h) = home.to_str() {
            if let Some(stripped) = p.strip_prefix(h) { return format!("~{stripped}"); }
        }
    }
    p.to_string()
}

pub fn render_bytes(r: &Receipt) -> Vec<u8> {
    render_bytes_with_qr(r, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn sample() -> Receipt {
        Receipt {
            scope: Scope::Session, agent: Agent::Claude,
            location: "Edinburgh, Scotland".into(), session_name: "twirling-melody".into(),
            project: Some("/tmp/proj".into()), git_branch: Some("main".into()),
            when: chrono::Utc.with_ymd_and_hms(2026,6,13,8,42,17).unwrap(),
            duration_secs: 4984,
            per_model: vec![ ModelLine{ model:"claude-opus-4-8".into(),
                input:274, output:3811, cache_write:828018, cache_read:9971315, cost:Some(10.07) } ],
            total_tokens: 12_759_816, total_cost: Some(10.65),
            cache_saved_tokens: 9971315, cache_saved_usd: Some(4.12),
            cache_hit_rate: 91.4, burn_rate_per_hr: Some(7.69),
            tools: vec![("Edit".into(),58),("Bash".into(),31)],
            git: GitStats{files_changed:12,added:1204,removed:317,commits:3},
            beads: BeadsStats{opened:vec!["tp-14".into()],closed:vec!["tp-9".into()]},
            sparkline: vec![1,2,3,5,7,6,4,3,2,1],
            precompact: false,
            billing_note: None,
        }
    }

    #[test]
    fn text_render_has_key_sections_and_width() {
        let s = render_text(&sample());
        assert!(s.contains("TOKEN PRINTER"));
        assert!(s.contains("MODEL BREAKDOWN"));
        assert!(s.contains("TOOL ACTIVITY"));
        assert!(s.contains("PRODUCTIVITY"));
        assert!(s.contains("TOTAL"));
        assert!(s.contains("$10.65"));
        assert!(s.contains("tp-14"));
        // every line <= 48 cols
        for line in s.lines() { assert!(line.chars().count() <= 48, "too wide: {line:?}"); }
    }

    #[test]
    fn long_location_does_not_overflow_48_cols() {
        let mut r = sample();
        // 60-char location — longer than the 48-col receipt width
        r.location = "A".repeat(60);
        let s = render_text(&r);
        for line in s.lines() {
            assert!(line.chars().count() <= 48,
                "line too wide ({} chars): {line:?}", line.chars().count());
        }
    }

    #[test]
    fn long_project_path_does_not_overflow_48_cols() {
        let mut r = sample();
        r.project = Some("/Users/aria/repos/reverie/.claude/worktrees/agent-a45f35040aa757f02".into());
        r.git_branch = Some("main".into());
        let s = render_text(&r);
        for line in s.lines() {
            assert!(line.chars().count() <= 48, "line too wide ({} chars): {line:?}", line.chars().count());
        }
    }

    #[test]
    fn bytes_render_starts_with_init_and_ends_with_cut() {
        let b = render_bytes(&sample());
        assert_eq!(&b[0..2], &[0x1b, 0x40]);          // ESC @
        assert_eq!(&b[b.len()-3..], &[0x1b,0x64,0x02]); // ESC d 2 cut
    }

    #[test]
    fn qr_bytes_emit_raster_and_keep_init_and_cut() {
        let r = sample();
        let with = render_bytes_with_qr(&r, Some("file:///tmp/session.jsonl"));
        assert_eq!(&with[0..2], &[0x1b, 0x40]);            // init
        assert_eq!(&with[with.len()-3..], &[0x1b,0x64,0x02]); // cut
        assert!(with.windows(3).any(|w| w == [0x1b,0x1d,0x53])); // raster cmd present
        assert!(with.len() > render_bytes(&r).len());      // QR added bytes
    }

    #[test]
    fn qr_raster_nonempty_for_normal_data() {
        assert!(!qr_raster("https://example.com/x").is_empty());
    }

    #[test]
    fn billing_note_some_renders_not_charged_and_all_lines_fit() {
        let mut r = sample();
        r.billing_note = Some("API-equivalent \u{2014} not charged on subscription".to_string());
        let s = render_text(&r);
        assert!(s.contains("not charged"), "expected 'not charged' in rendered output");
        for line in s.lines() {
            assert!(
                line.chars().count() <= 48,
                "line too wide ({} chars): {line:?}", line.chars().count()
            );
        }
    }

    #[test]
    fn billing_note_none_does_not_render_not_charged() {
        let mut r = sample();
        r.billing_note = None;
        let s = render_text(&r);
        assert!(!s.contains("not charged"), "expected no billing note in api mode");
    }
}
