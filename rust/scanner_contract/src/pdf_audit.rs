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

pub fn pdf_sample_pages(page_count: u64, max_pages: u64, policy: &str) -> Vec<u64> {
    let count = page_count.min(max_pages);
    if count == 0 {
        return Vec::new();
    }
    if policy == "pdf_text_presence_v1" {
        return (1..=count).collect();
    }
    let mut pages: Vec<u64> = (1..=count.min(2)).collect();
    if pages.len() < count as usize && !pages.contains(&page_count) {
        pages.push(page_count);
    }
    let remaining = count - pages.len() as u64;
    for i in 1..=remaining {
        let page = 1 + (page_count - 1) * i / (remaining + 1);
        if !pages.contains(&page) {
            pages.push(page);
        }
    }
    for page in 1..=page_count {
        if pages.len() == count as usize {
            break;
        }
        if !pages.contains(&page) {
            pages.push(page);
        }
    }
    pages
}

impl PdfPageAudit {
    pub fn validate(&self) -> Result<(), String> {
        if !matches!(
            self.policy_version.as_str(),
            "pdf_text_presence_v1" | "pdf_text_presence_v2"
        ) || self.checked_pages.len() > 10_000
            || self.checked_pages.contains(&0)
            || self.checked_pages.iter().collect::<HashSet<_>>().len() != self.checked_pages.len()
        {
            return Err("invalid PDF page audit".into());
        }
        if self.observations.as_ref().is_some_and(|items| {
            items.iter().map(|item| item.page).collect::<Vec<_>>() != self.checked_pages
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
    }
}
