//! A small unified line diff for `moi diff`. State files are configuration-sized; inputs too large for
//! the quadratic table are summarized instead of diffed.

/// Beyond this many table cells (old lines × new lines) a diff is not computed.
const MAX_CELLS: usize = 16_000_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep,
    Remove,
    Add,
}

/// Unified diff body (`@@` hunks) of two texts with `context` unchanged lines around each change. Empty
/// when the texts are equal.
pub fn unified(old: &str, new: &str, context: usize) -> String {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    // common prefix and suffix cost nothing and are most of a typical change
    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..].iter().rev().zip(b[prefix..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (mid_a, mid_b) = (&a[prefix..a.len() - suffix], &b[prefix..b.len() - suffix]);
    if mid_a.is_empty() && mid_b.is_empty() {
        return String::new();
    }
    if mid_a.len().saturating_mul(mid_b.len()) > MAX_CELLS {
        return format!("@@ too large to diff: {} lines became {} lines @@\n", a.len(), b.len());
    }

    // longest common subsequence, filled from the end so the walk below runs forward
    let (n, m) = (mid_a.len(), mid_b.len());
    let mut table = vec![0u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i * (m + 1) + j] = if mid_a[i] == mid_b[j] {
                table[(i + 1) * (m + 1) + j + 1] + 1
            } else {
                table[(i + 1) * (m + 1) + j].max(table[i * (m + 1) + j + 1])
            };
        }
    }
    let mut ops: Vec<(Op, &str)> = a[..prefix].iter().map(|l| (Op::Keep, *l)).collect();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && mid_a[i] == mid_b[j] {
            ops.push((Op::Keep, mid_a[i]));
            i += 1;
            j += 1;
        } else if j < m && (i == n || table[i * (m + 1) + j + 1] > table[(i + 1) * (m + 1) + j]) {
            ops.push((Op::Add, mid_b[j]));
            j += 1;
        } else {
            ops.push((Op::Remove, mid_a[i]));
            i += 1;
        }
    }
    ops.extend(a[a.len() - suffix..].iter().map(|l| (Op::Keep, *l)));

    // group changes into hunks with their context
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, (op, _))| *op != Op::Keep).map(|(i, _)| i).collect();
    let mut out = String::new();
    let mut k = 0;
    while k < changed.len() {
        let start = changed[k].saturating_sub(context);
        let mut end = (changed[k] + context + 1).min(ops.len());
        while k + 1 < changed.len() && changed[k + 1] <= end + context {
            k += 1;
            end = (changed[k] + context + 1).min(ops.len());
        }
        k += 1;
        let before = |op: Op| ops[..start].iter().filter(|(o, _)| *o != op).count();
        let within = |op: Op| ops[start..end].iter().filter(|(o, _)| *o != op).count();
        // line numbers are 1-based; an empty range is written at the line before it
        let range =
            |first: usize, len: usize| if len == 0 { format!("{first},0") } else { format!("{},{len}", first + 1) };
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            range(before(Op::Add), within(Op::Add)),
            range(before(Op::Remove), within(Op::Remove))
        ));
        for (op, line) in &ops[start..end] {
            out.push(match op {
                Op::Keep => ' ',
                Op::Remove => '-',
                Op::Add => '+',
            });
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}
