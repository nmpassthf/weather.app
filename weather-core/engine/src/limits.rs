use weather_schema::{MAX_RPC_PAGE_OFFSET, MAX_RPC_PAGE_SIZE};

pub(crate) const MAX_RPC_PAYLOAD_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_CONCURRENT_REQUESTS: usize = 64;
pub(crate) const MAX_CONCURRENT_REFRESHES: usize = 4;
pub(crate) const MAX_CONCURRENT_CATALOG_FETCHES: usize = 8;
pub(crate) const MAX_BATCH_QUERIES: usize = 64;
pub(crate) const DEFAULT_PAGE_SIZE: u32 = 32;
pub(crate) const DEFAULT_FUZZY_PAGE_SIZE: u32 = 10;

pub(crate) fn normalize_pagination(
    offset: u32,
    page_size: u32,
    default_page_size: u32,
) -> Result<(usize, usize), String> {
    if offset > MAX_RPC_PAGE_OFFSET {
        return Err(format!(
            "page_offset {offset} exceeds maximum {MAX_RPC_PAGE_OFFSET}"
        ));
    }
    let page_size = if page_size == 0 {
        default_page_size
    } else {
        page_size
    };
    if page_size > MAX_RPC_PAGE_SIZE {
        return Err(format!(
            "page_size {page_size} exceeds maximum {MAX_RPC_PAGE_SIZE}"
        ));
    }
    Ok((offset as usize, page_size as usize))
}

pub(crate) fn validate_batch_size(len: usize) -> Result<(), String> {
    if len > MAX_BATCH_QUERIES {
        Err(format!(
            "batch query count {len} exceeds maximum {MAX_BATCH_QUERIES}"
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagination_applies_default_and_accepts_bounds() {
        assert_eq!(
            normalize_pagination(MAX_RPC_PAGE_OFFSET, 0, DEFAULT_PAGE_SIZE),
            Ok((MAX_RPC_PAGE_OFFSET as usize, DEFAULT_PAGE_SIZE as usize))
        );
        assert_eq!(
            normalize_pagination(0, MAX_RPC_PAGE_SIZE, DEFAULT_PAGE_SIZE),
            Ok((0, MAX_RPC_PAGE_SIZE as usize))
        );
    }

    #[test]
    fn pagination_rejects_oversized_values() {
        assert!(normalize_pagination(MAX_RPC_PAGE_OFFSET + 1, 1, DEFAULT_PAGE_SIZE).is_err());
        assert!(normalize_pagination(0, MAX_RPC_PAGE_SIZE + 1, DEFAULT_PAGE_SIZE).is_err());
    }

    #[test]
    fn batch_limit_is_inclusive() {
        assert!(validate_batch_size(MAX_BATCH_QUERIES).is_ok());
        assert!(validate_batch_size(MAX_BATCH_QUERIES + 1).is_err());
    }
}
