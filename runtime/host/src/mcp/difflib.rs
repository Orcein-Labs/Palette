//! Python's `difflib.unified_diff` with `SequenceMatcher`'s default
//! heuristics, line for line, so a patch preview reads the same whichever
//! host prepared it.

use std::collections::HashMap;

/// `s.splitlines(keepends=True)`: every line boundary Python's `str` knows.
pub(crate) fn split_lines_keep(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let end = match c {
            '\r' if chars.peek().is_some_and(|&(_, next)| next == '\n') => {
                chars.next();
                i + 2
            }
            '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}' => i + c.len_utf8(),
            _ => continue,
        };
        lines.push(&s[start..end]);
        start = end;
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Tag {
    Equal,
    Replace,
    Delete,
    Insert,
}

type Opcode = (Tag, usize, usize, usize, usize);

struct Matcher<'a> {
    a: &'a [&'a str],
    b: &'a [&'a str],
    b2j: HashMap<&'a str, Vec<usize>>,
}

impl<'a> Matcher<'a> {
    fn new(a: &'a [&'a str], b: &'a [&'a str]) -> Matcher<'a> {
        let mut b2j: HashMap<&str, Vec<usize>> = HashMap::new();
        for (j, line) in b.iter().enumerate() {
            b2j.entry(line).or_default().push(j);
        }
        // autojunk: a line in more than 1% of a long `b` is too popular to
        // anchor a match (it can still extend one).
        if b.len() >= 200 {
            let ntest = b.len() / 100 + 1;
            b2j.retain(|_, indices| indices.len() <= ntest);
        }
        Matcher { a, b, b2j }
    }

    fn longest_match(&self, alo: usize, ahi: usize, blo: usize, bhi: usize) -> (usize, usize, usize) {
        let (mut besti, mut bestj, mut bestsize) = (alo, blo, 0);
        let mut j2len: HashMap<usize, usize> = HashMap::new();
        for i in alo..ahi {
            let mut next = HashMap::new();
            for &j in self.b2j.get(self.a[i]).map(Vec::as_slice).unwrap_or_default() {
                if j < blo {
                    continue;
                }
                if j >= bhi {
                    break;
                }
                let k = j.checked_sub(1).and_then(|p| j2len.get(&p)).copied().unwrap_or(0) + 1;
                next.insert(j, k);
                if k > bestsize {
                    (besti, bestj, bestsize) = (i + 1 - k, j + 1 - k, k);
                }
            }
            j2len = next;
        }
        while besti > alo && bestj > blo && self.a[besti - 1] == self.b[bestj - 1] {
            (besti, bestj, bestsize) = (besti - 1, bestj - 1, bestsize + 1);
        }
        while besti + bestsize < ahi && bestj + bestsize < bhi && self.a[besti + bestsize] == self.b[bestj + bestsize] {
            bestsize += 1;
        }
        (besti, bestj, bestsize)
    }

    fn matching_blocks(&self) -> Vec<(usize, usize, usize)> {
        let (la, lb) = (self.a.len(), self.b.len());
        let mut queue = vec![(0, la, 0, lb)];
        let mut blocks = Vec::new();
        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let (i, j, k) = self.longest_match(alo, ahi, blo, bhi);
            if k > 0 {
                blocks.push((i, j, k));
                if alo < i && blo < j {
                    queue.push((alo, i, blo, j));
                }
                if i + k < ahi && j + k < bhi {
                    queue.push((i + k, ahi, j + k, bhi));
                }
            }
        }
        blocks.sort();
        let (mut i1, mut j1, mut k1) = (0, 0, 0);
        let mut merged = Vec::new();
        for (i2, j2, k2) in blocks {
            if i1 + k1 == i2 && j1 + k1 == j2 {
                k1 += k2;
            } else {
                if k1 > 0 {
                    merged.push((i1, j1, k1));
                }
                (i1, j1, k1) = (i2, j2, k2);
            }
        }
        if k1 > 0 {
            merged.push((i1, j1, k1));
        }
        merged.push((la, lb, 0));
        merged
    }

    fn opcodes(&self) -> Vec<Opcode> {
        let (mut i, mut j) = (0, 0);
        let mut codes = Vec::new();
        for (ai, bj, size) in self.matching_blocks() {
            let tag = match (i < ai, j < bj) {
                (true, true) => Some(Tag::Replace),
                (true, false) => Some(Tag::Delete),
                (false, true) => Some(Tag::Insert),
                (false, false) => None,
            };
            if let Some(tag) = tag {
                codes.push((tag, i, ai, j, bj));
            }
            (i, j) = (ai + size, bj + size);
            if size > 0 {
                codes.push((Tag::Equal, ai, i, bj, j));
            }
        }
        codes
    }

    fn grouped_opcodes(&self, n: usize) -> Vec<Vec<Opcode>> {
        let mut codes = self.opcodes();
        if codes.is_empty() {
            codes.push((Tag::Equal, 0, 1, 0, 1));
        }
        if let Some(first) = codes.first_mut().filter(|c| c.0 == Tag::Equal) {
            let (tag, i1, i2, j1, j2) = *first;
            *first = (tag, i1.max(i2.saturating_sub(n)), i2, j1.max(j2.saturating_sub(n)), j2);
        }
        if let Some(last) = codes.last_mut().filter(|c| c.0 == Tag::Equal) {
            let (tag, i1, i2, j1, j2) = *last;
            *last = (tag, i1, i2.min(i1 + n), j1, j2.min(j1 + n));
        }
        let mut groups = Vec::new();
        let mut group = Vec::new();
        for (tag, mut i1, i2, mut j1, j2) in codes {
            // A long unchanged run ends one hunk and starts the next.
            if tag == Tag::Equal && i2 - i1 > 2 * n {
                group.push((tag, i1, i2.min(i1 + n), j1, j2.min(j1 + n)));
                groups.push(std::mem::take(&mut group));
                (i1, j1) = (i1.max(i2.saturating_sub(n)), j1.max(j2.saturating_sub(n)));
            }
            group.push((tag, i1, i2, j1, j2));
        }
        if !group.is_empty() && !(group.len() == 1 && group[0].0 == Tag::Equal) {
            groups.push(group);
        }
        groups
    }
}

fn unified_range(start: usize, stop: usize) -> String {
    let length = stop - start;
    match length {
        1 => format!("{}", start + 1),
        0 => format!("{start},0"),
        _ => format!("{},{length}", start + 1),
    }
}

/// `"".join(difflib.unified_diff(a, b, fromfile, tofile))` with three lines
/// of context; `a` and `b` are lines with their endings.
pub(crate) fn unified_diff(a: &[&str], b: &[&str], fromfile: &str, tofile: &str) -> String {
    let mut out = String::new();
    for (k, group) in Matcher::new(a, b).grouped_opcodes(3).iter().enumerate() {
        if k == 0 {
            out.push_str(&format!("--- {fromfile}\n+++ {tofile}\n"));
        }
        let (first, last) = (group[0], group[group.len() - 1]);
        out.push_str(&format!("@@ -{} +{} @@\n", unified_range(first.1, last.2), unified_range(first.3, last.4)));
        for &(tag, i1, i2, j1, j2) in group {
            if tag == Tag::Equal {
                a[i1..i2].iter().for_each(|line| out.extend([" ", line]));
                continue;
            }
            if matches!(tag, Tag::Replace | Tag::Delete) {
                a[i1..i2].iter().for_each(|line| out.extend(["-", line]));
            }
            if matches!(tag, Tag::Replace | Tag::Insert) {
                b[j1..j2].iter().for_each(|line| out.extend(["+", line]));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_split_where_python_splits_them() {
        assert_eq!(split_lines_keep(""), Vec::<&str>::new());
        assert_eq!(split_lines_keep("a\r\nb\rc\nd"), ["a\r\n", "b\r", "c\n", "d"]);
        assert_eq!(split_lines_keep("x\x0by\u{2028}z\u{85}"), ["x\x0b", "y\u{2028}", "z\u{85}"]);
        assert_eq!(split_lines_keep("\n\n"), ["\n", "\n"]);
    }

    fn diff(a: &str, b: &str) -> String {
        unified_diff(&split_lines_keep(a), &split_lines_keep(b), "f", "f")
    }

    const DIFF_REPEATS: &str = "--- f\n+++ f\n@@ -1,40 +1,40 @@\n 0\n+3\n+6\n+2\n+5\n 1\n+4\n+0\n+3\n+6\n 2\n+5\n+1\n+4\n+0\n 3\n+6\n+2\n+5\n+1\n 4\n+0\n+3\n+6\n+2\n 5\n+1\n+4\n+0\n+3\n 6\n+2\n+5\n+1\n+4\n 0\n-1\n+3\n+6\n 2\n-3\n-4\n 5\n-6\n-0\n-1\n-2\n-3\n-4\n-5\n-6\n-0\n-1\n-2\n-3\n-4\n-5\n-6\n-0\n-1\n-2\n-3\n-4\n-5\n-6\n-0\n-1\n-2\n-3\n-4\n";

    // Expected outputs are what CPython 3.14's difflib prints.
    #[test]
    fn diffs_match_python() {
        assert_eq!(diff("", ""), "");
        assert_eq!(diff("same\n", "same\n"), "");
        assert_eq!(diff("", "new\n"), "--- f\n+++ f\n@@ -0,0 +1 @@\n+new\n");
        assert_eq!(diff("before\n", "after\n"), "--- f\n+++ f\n@@ -1 +1 @@\n-before\n+after\n");
        assert_eq!(diff("a\nb", "a\nc"), "--- f\n+++ f\n@@ -1,2 +1,2 @@\n a\n-b+c");
        let old: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let new = old.replace("2\n", "two\n").replace("\n18\n", "\neighteen\n");
        assert_eq!(diff(&old, &new), "--- f\n+++ f\n@@ -1,5 +1,5 @@\n 1\n-2\n+two\n 3\n 4\n 5\n\
            @@ -9,12 +9,12 @@\n 9\n 10\n 11\n-12\n+1two\n 13\n 14\n 15\n 16\n 17\n-18\n+eighteen\n 19\n 20\n");
        let old: String = (0..40).map(|i| format!("{}\n", i % 7)).collect();
        let new: String = (0..40).map(|i| format!("{}\n", (i * 3) % 7)).collect();
        assert_eq!(diff(&old, &new), DIFF_REPEATS);
    }
}
