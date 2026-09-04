#![allow(clippy::expect_used, clippy::unwrap_used)]

use lege_pdf_agent::pages::{parse_bbox, parse_one_based, parse_page_range};

#[test]
fn one_based_only() {
    assert!(parse_one_based("0").is_err());
    assert_eq!(parse_one_based("1").unwrap(), 1);
}

#[test]
fn range_dedup_and_order() {
    let (pages, warnings) = parse_page_range(Some("3,1-2,2"), 5, 50).unwrap();
    assert!(warnings.is_empty());
    assert_eq!(pages.iter().map(|p| p.0).collect::<Vec<_>>(), vec![0, 1, 2]);
}

#[test]
fn max_pages_truncates() {
    let (pages, warnings) = parse_page_range(Some("all"), 100, 10).unwrap();
    assert_eq!(pages.len(), 10);
    assert!(!warnings.is_empty());
}

#[test]
fn bbox_parse() {
    assert_eq!(parse_bbox("10,20,30,40").unwrap(), [10.0, 20.0, 30.0, 40.0]);
    assert!(parse_bbox("1,2,3").is_err());
}
