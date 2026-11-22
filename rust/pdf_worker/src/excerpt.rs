use crate::{is_valid_text_char, PdfError};
use std::collections::{BTreeMap, BinaryHeap};

mod layout;
pub(super) use layout::{format_page_text, page_bounds, read_page_text};

// AuditTextBuilder can record at most 512 complete units, including their page ranges.
const MAX_LINES: usize = 512;

pub(super) struct PageText {
    pub page: u64,
    pub text: String,
    truncated: bool,
}

pub(super) struct PdfText {
    pub pages: Vec<PageText>,
    pub truncated: bool,
}

#[derive(Clone, Copy)]
struct Budget {
    chars: usize,
    lines: usize,
}

impl Budget {
    fn take(&mut self, cost: usize) -> bool {
        if self.lines == 0 || cost > self.chars {
            return false;
        }
        self.lines -= 1;
        self.chars -= cost;
        true
    }
}

struct Candidate {
    index: usize,
    cost: usize,
    text: String,
    selected: bool,
}

impl Candidate {
    fn new(index: usize, cost: usize, text: &str) -> Self {
        Self {
            index,
            cost,
            text: text.to_owned(),
            selected: false,
        }
    }
}

#[derive(Default)]
struct Page {
    number: u64,
    lines_total: usize,
    shortest: Option<Candidate>,
    candidates: Vec<Candidate>,
    selected: usize,
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    text.split(['\r', '\n'])
        .map(str::trim)
        .filter(|line| line.chars().any(is_valid_text_char))
}

pub(super) fn read_pages(
    total: u64,
    numbers: &[u64],
    max_chars: usize,
    mut extract: impl FnMut(u64) -> Result<String, PdfError>,
) -> Result<PdfText, PdfError> {
    // A newline is charged to every line, with one free leading newline for the document.
    let limit = Budget {
        chars: max_chars.saturating_add(1),
        lines: MAX_LINES,
    };
    let share = Budget {
        chars: limit.chars.div_ceil(numbers.len()),
        lines: limit.lines.div_ceil(numbers.len()),
    };
    let mut overflow = BTreeMap::new();
    let mut overflow_budget = limit;
    let mut reservations = BinaryHeap::new();
    let mut reserved_chars = 0usize;
    let mut preview = None;
    let mut pages = Vec::with_capacity(numbers.len());

    for &number in numbers {
        let text = extract(number)?;
        let mut page = Page {
            number,
            ..Page::default()
        };
        let mut local = share;
        let mut extra_index = 0;
        let mut shortest: Option<(usize, usize, &str)> = None;
        for (index, line) in lines(&text).enumerate() {
            let cost = line.chars().count() + 1;
            page.lines_total += 1;
            if cost <= limit.chars && shortest.is_none_or(|(_, old, _)| cost < old) {
                shortest = Some((index, cost, line));
            }
            if local.take(cost) {
                page.candidates.push(Candidate::new(index, cost, line));
            } else if cost <= limit.chars {
                // Earlier overflow lines on another page outrank a long cover's later lines.
                // This retains refill candidates even when the remaining pages turn out blank.
                let rank = (extra_index, pages.len());
                extra_index += 1;
                if make_room(&mut overflow, &mut overflow_budget, rank, cost) {
                    overflow.insert(rank, Candidate::new(index, cost, line));
                }
            }
            if index == 0 && cost > limit.chars && reservations.is_empty() && preview.is_none() {
                preview = safe_preview(line, max_chars).map(|value| (number, value.to_owned()));
            }
        }

        if let Some((index, cost, line)) = shortest {
            let page_index = pages.len();
            page.shortest = Some(Candidate::new(index, cost, line));
            reservations.push((cost, page_index));
            reserved_chars += cost;
        }
        pages.push(page);
        // Hold at most one output budget of page minima, even for 10,000 large pages.
        // For equal costs the earlier requested page keeps its reservation.
        while reserved_chars > limit.chars || reservations.len() > limit.lines {
            let (cost, index) = reservations.pop().expect("over budget with a reservation");
            reserved_chars -= cost;
            pages[index].shortest = None;
        }
    }

    for ((_, page), line) in overflow {
        pages[page].candidates.push(line);
    }
    let mut spare = Budget {
        chars: limit.chars - reserved_chars,
        lines: limit.lines - reservations.len(),
    };
    for page in &mut pages {
        let Some(minimum) = page.shortest.take() else {
            continue;
        };
        page.candidates.sort_unstable_by_key(|line| line.index);
        let cost = minimum.cost;
        let position = match page
            .candidates
            .binary_search_by_key(&minimum.index, |line| line.index)
        {
            Ok(position) => position,
            Err(position) => {
                page.candidates.insert(position, minimum);
                position
            }
        };
        let preferred = page
            .candidates
            .iter()
            .position(|line| line.cost <= share.chars && line.cost - cost <= spare.chars)
            .unwrap_or(position);
        spare.chars -= page.candidates[preferred].cost - cost;
        page.candidates[preferred].selected = true;
        page.selected = 1;
    }

    let mut claimants = pages
        .iter()
        .filter(|p| p.selected > 0 && p.candidates.len() > 1)
        .count();
    for page in pages
        .iter_mut()
        .filter(|p| p.selected > 0 && p.candidates.len() > 1)
    {
        let mut allowance = Budget {
            chars: spare.chars.div_ceil(claimants),
            lines: spare.lines.div_ceil(claimants),
        };
        claimants -= 1;
        let before = allowance;
        page.fill(&mut allowance);
        spare.chars -= before.chars - allowance.chars;
        spare.lines -= before.lines - allowance.lines;
    }
    // Reclaim unused shares from the already retained candidates, in requested page order.
    let mut output = Vec::with_capacity(pages.len());
    for mut page in pages {
        if page.selected > 0 {
            page.fill(&mut spare);
        }
        let mut text = page
            .candidates
            .into_iter()
            .filter(|line| line.selected)
            .map(|line| line.text)
            .collect::<Vec<_>>()
            .join("\n");
        if reservations.is_empty()
            && preview
                .as_ref()
                .is_some_and(|(number, _)| *number == page.number)
        {
            text = preview.take().unwrap().1;
        }
        output.push(PageText {
            page: page.number,
            truncated: page.selected < page.lines_total,
            text,
        });
    }
    Ok(PdfText {
        truncated: output.len() < total as usize || output.iter().any(|p| p.truncated),
        pages: output,
    })
}

fn make_room(
    pool: &mut BTreeMap<(usize, usize), Candidate>,
    free: &mut Budget,
    rank: (usize, usize),
    cost: usize,
) -> bool {
    if free.take(cost) {
        return true;
    }
    let mut available = *free;
    for (i, (_, line)) in pool.range(rank..).rev().enumerate() {
        available.chars += line.cost;
        available.lines += 1;
        if available.take(cost) {
            for _ in 0..=i {
                pool.pop_last();
            }
            *free = available;
            return true;
        }
    }
    false
}

impl Page {
    fn fill(&mut self, budget: &mut Budget) {
        for line in &mut self.candidates {
            if budget.lines == 0 || budget.chars <= 1 {
                break;
            }
            if !line.selected && budget.take(line.cost) {
                line.selected = true;
                self.selected += 1;
            }
        }
    }
}

// A partial numeric clause can lose a decimal, multiplier, unit or condition.
// Only emit a numeric claim at a complete sentence boundary (or as a whole line).
fn safe_preview(line: &str, max_chars: usize) -> Option<&str> {
    let cut = line
        .char_indices()
        .nth(max_chars)
        .map_or(line.len(), |(byte, _)| byte);
    if cut == line.len() {
        return Some(line);
    }
    // Keep a table row and its value together.
    if line.contains('\t') {
        return None;
    }
    let prefix = &line[..cut];
    let sentence_end = prefix
        .char_indices()
        .filter_map(|(i, c)| {
            let end = i + c.len_utf8();
            let complete = match c {
                '。' | '！' | '？' | '；' | ';' => true,
                '!' | '?' => line[end..].starts_with(char::is_whitespace),
                '.' if line[end..].starts_with(char::is_whitespace) => {
                    // Avoid dotted abbreviations, numbered list prefixes and decimal separators.
                    line[..i]
                        .split_whitespace()
                        .next_back()
                        .is_some_and(|word| {
                            if word.chars().any(char::is_numeric) {
                                let next =
                                    line[end..].split_whitespace().next().unwrap_or_default();
                                line[..i - word.len()].chars().any(char::is_alphabetic)
                                    && next.starts_with(char::is_uppercase)
                                    && next.chars().any(char::is_lowercase)
                            } else {
                                word.chars().last().is_some_and(char::is_alphabetic)
                                    && word.chars().count() > 3
                                    && !word.contains('.')
                            }
                        })
                }
                _ => false,
            };
            complete.then_some(end)
        })
        .next_back();
    if let Some(end) = sentence_end {
        return Some(&line[..end]);
    }

    let mut pos = 0;
    let mut complete = 0;
    for part in line.split_inclusive(char::is_whitespace) {
        let word = part.trim_end();
        let quantitative = word.chars().any(|c| {
            c.is_numeric()
                || "零〇一二两三四五六七八九十百千万亿壹贰叁肆伍陆柒捌玖拾佰仟".contains(c)
        });
        if quantitative
            || (!word.is_empty() && !word.chars().any(char::is_alphabetic))
            || matches!(
                word,
                "USD" | "CNY" | "RMB" | "EUR" | "GBP" | "JPY" | "HKD" | "人民币"
            )
        {
            break;
        }
        if pos + word.len() > cut {
            if complete == 0 && word.starts_with(char::is_alphabetic) {
                return Some(prefix.trim_end()).filter(|s| s.chars().any(is_valid_text_char));
            }
            break;
        }
        if !word.is_empty() {
            complete = pos + word.len();
        }
        pos += part.len();
    }
    (complete > 0).then(|| &line[..complete])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn previews_keep_complete_numeric_sentences_and_words() {
        for value in [
            "应付 12345 元",
            "应付 123.45 万元",
            "应付 １２０ 公斤",
            "Due USD 12,345.67",
            "Due -123.45 kg",
            "Due 8.25 %",
        ] {
            for limit in 1..value.chars().count() {
                let preview = safe_preview(value, limit).unwrap_or("");
                assert!(value.starts_with(preview) && preview.chars().count() <= limit);
                assert!(
                    !preview.chars().any(char::is_numeric),
                    "{value:?} ({limit}) -> {preview:?}"
                );
            }
            assert_eq!(safe_preview(value, value.chars().count()), Some(value));
        }
        let sentence = "应付 12345 元。";
        assert_eq!(
            safe_preview("应付 12345 元。验收后支付", sentence.chars().count()),
            Some(sentence)
        );
        let sentence = "Paid USD 1,234.50.";
        assert_eq!(
            safe_preview("Paid USD 1,234.50. More details follow.", sentence.len()),
            Some(sentence)
        );
        assert_eq!(safe_preview("Review pending approval", 10), Some("Review"));
        assert_eq!(safe_preview("Quarterly financial review", 5), Some("Quart"));
        assert_eq!(safe_preview("12345 kg", 5), None);
        assert_eq!(safe_preview("Freight\t12345 USD", 13), None);
    }

    #[test]
    fn dense_pages_are_extracted_once_and_keep_late_fitting_lines() {
        let mut calls = Vec::new();
        let parsed = read_pages(7, &[1, 7, 3], 35, |number| {
            calls.push(number);
            Ok(match number {
                1 => format!(
                    "{}cover note",
                    "a long line that does not fit the character limit\n".repeat(3000)
                ),
                7 => "Final 4817 USD".to_string(),
                _ => "On review".to_string(),
            })
        })
        .unwrap();
        assert_eq!(calls, [1, 7, 3]);
        assert!(parsed.truncated && parsed.pages[0].truncated);
        assert_eq!(
            parsed
                .pages
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>(),
            ["cover note", "Final 4817 USD", "On review"]
        );
    }
}
