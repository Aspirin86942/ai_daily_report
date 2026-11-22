use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfPageObservation {
    pub page: u64,
    pub has_text: bool,
    pub has_image: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PdfPageAudit {
    pub policy_version: String,
    pub checked_pages: Vec<u64>,
    pub observations: Option<Vec<PdfPageObservation>>,
}

pub(crate) fn validate_pdf_policy(policy: &str) -> Result<(), String> {
    match policy {
        "pdf_text_presence_v1" | "pdf_text_presence_v2" => Ok(()),
        _ => Err("unsupported PDF selection policy".into()),
    }
}

/// A text hit may end the inspection early. A negative result needs the complete window.
pub(crate) fn validate_text_window(
    page_count: Option<u64>,
    examined: Option<u64>,
    limit: u64,
    has_text: bool,
) -> Result<(), String> {
    let window = page_count.unwrap_or(0).min(limit);
    let examined = examined.unwrap_or(0);
    if !(1..=window).contains(&examined) || (!has_text && examined != window) {
        return Err("PDF page evidence does not cover the classification window".into());
    }
    Ok(())
}

pub fn pdf_sample_pages(page_count: u64, max_pages: u64, policy: &str) -> Vec<u64> {
    let count = page_count.min(max_pages);
    if count <= 2 || policy == "pdf_text_presence_v1" {
        return (1..=count).collect();
    }
    let mut pages = vec![1, 2, page_count];
    let remaining = count - 3;
    for i in 1..=remaining {
        let offset = u128::from(page_count - 1) * u128::from(i) / u128::from(remaining + 1);
        let page = 1 + offset as u64;
        // Interior samples strictly increase. Only the first can coincide with page two.
        if page > 2 {
            pages.push(page);
        }
    }
    if pages.len() < count as usize {
        let mut missing = 3;
        for &page in &pages[3..] {
            if page != missing {
                break;
            }
            missing += 1;
        }
        pages.push(missing);
    }
    pages
}

pub fn pdf_extraction_pages(
    page_count: u64,
    limit: u64,
    first_text: Option<u64>,
    selected: Option<&[u64]>,
    policy: &str,
) -> Vec<u64> {
    if let Some(pages) = selected {
        return pages.to_vec();
    }
    if policy == "pdf_text_presence_v2" && limit == 2 && page_count > 2 {
        let first = first_text.unwrap_or(1);
        return vec![first, if first == page_count { 1 } else { page_count }];
    }
    pdf_sample_pages(page_count, limit, policy)
}

impl PdfPageAudit {
    pub(crate) fn validate_counts(
        &self,
        page_count: Option<u64>,
        examined: Option<u64>,
    ) -> Result<(), String> {
        self.validate()?;
        if examined != Some(self.checked_pages.len() as u64)
            || page_count.is_none_or(|count| self.checked_pages.iter().any(|page| *page > count))
        {
            return Err("PDF observed pages violate page counts".into());
        }
        Ok(())
    }

    pub fn all_pages_are_images(&self, page_count: u64) -> bool {
        let Some(observations) = &self.observations else {
            return false;
        };
        // A count of distinct pages within 1..=page_count also covers non-sequential sampling.
        page_count > 0
            && self
                .validate_counts(Some(page_count), Some(page_count))
                .is_ok()
            && observations
                .iter()
                .all(|page| !page.has_text && page.has_image == Some(true))
    }

    pub fn validate(&self) -> Result<(), String> {
        let mut seen = HashSet::new();
        validate_pdf_policy(&self.policy_version)?;
        if self.checked_pages.len() > 10_000
            || self
                .checked_pages
                .iter()
                .any(|&page| page == 0 || !seen.insert(page))
        {
            return Err("invalid PDF page audit".into());
        }
        if self.observations.as_ref().is_some_and(|items| {
            items
                .iter()
                .map(|item| item.page)
                .ne(self.checked_pages.iter().copied())
        }) {
            return Err("PDF observations do not match checked pages".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sampled_pages_are_bounded_distinct_and_include_the_last_page() {
        for count in 1..35 {
            for limit in 1..12 {
                let pages = pdf_sample_pages(count, limit, "pdf_text_presence_v2");
                assert_eq!(pages.len(), count.min(limit) as usize);
                assert_eq!(pages.iter().collect::<HashSet<_>>().len(), pages.len());
                assert_eq!(pages[0], 1);
                if count > 2 && limit >= 3 {
                    assert_eq!(pages[2], count);
                }
            }
        }
        assert_eq!(
            pdf_sample_pages(8, 5, "pdf_text_presence_v2"),
            vec![1, 2, 8, 3, 5]
        );
        assert_eq!(
            pdf_sample_pages(6, 6, "pdf_text_presence_v2"),
            [1, 2, 6, 3, 4, 5]
        );
        let pages = pdf_sample_pages(u64::MAX, 6, "pdf_text_presence_v2");
        assert_eq!(pages, [1, 2, u64::MAX, 1 << 62, 1 << 63, (3u64 << 62) - 1]);
    }
}
