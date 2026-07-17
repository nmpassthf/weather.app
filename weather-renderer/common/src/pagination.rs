use anyhow::{Result, bail};
use weather_schema::{MAX_RPC_PAGE_OFFSET, MAX_RPC_PAGE_SIZE};

/// A logical client operation cannot need more full-sized requests than the
/// server's entire supported offset range, plus its initial/final request.
const MAX_PAGINATED_REQUESTS: u32 = MAX_RPC_PAGE_OFFSET / MAX_RPC_PAGE_SIZE + 2;

#[derive(Debug, Default)]
pub struct PageCursor {
    offset: u32,
    request_count: u32,
}

impl PageCursor {
    pub fn request(&mut self, page_size: u32) -> Result<(u32, u32)> {
        if page_size == 0 || page_size > MAX_RPC_PAGE_SIZE {
            bail!("RPC page size {page_size} is outside 1..={MAX_RPC_PAGE_SIZE}");
        }
        if self.offset > MAX_RPC_PAGE_OFFSET {
            bail!(
                "RPC page offset {} exceeds maximum {MAX_RPC_PAGE_OFFSET}",
                self.offset
            );
        }
        if self.request_count >= MAX_PAGINATED_REQUESTS {
            bail!("RPC pagination exceeded {MAX_PAGINATED_REQUESTS} requests");
        }
        self.request_count += 1;
        Ok((self.offset, page_size))
    }

    pub fn advance(&mut self, has_more: bool, next_offset: u32) -> Result<bool> {
        if !has_more {
            return Ok(false);
        }
        if next_offset <= self.offset {
            bail!(
                "RPC pagination did not advance: current offset {}, next offset {next_offset}",
                self.offset
            );
        }
        if next_offset > MAX_RPC_PAGE_OFFSET {
            bail!("RPC pagination next offset {next_offset} exceeds maximum {MAX_RPC_PAGE_OFFSET}");
        }
        self.offset = next_offset;
        Ok(true)
    }
}

pub fn page_size_for_target(target: usize) -> u32 {
    target.clamp(1, MAX_RPC_PAGE_SIZE as usize) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_size_is_always_within_rpc_bounds() {
        assert_eq!(page_size_for_target(0), 1);
        assert_eq!(page_size_for_target(1), 1);
        assert_eq!(page_size_for_target(600), MAX_RPC_PAGE_SIZE);
    }

    #[test]
    fn cursor_rejects_non_advancing_offsets() {
        let mut cursor = PageCursor::default();
        cursor.request(10).unwrap();

        let error = cursor.advance(true, 0).unwrap_err();

        assert!(error.to_string().contains("did not advance"));
    }

    #[test]
    fn cursor_rejects_offsets_beyond_the_server_scan_bound() {
        let mut cursor = PageCursor::default();
        cursor.request(MAX_RPC_PAGE_SIZE).unwrap();

        let error = cursor.advance(true, MAX_RPC_PAGE_OFFSET + 1).unwrap_err();

        assert!(error.to_string().contains("exceeds maximum"));
    }

    #[test]
    fn cursor_caps_the_total_request_count() {
        let mut cursor = PageCursor::default();
        for next in 1..=MAX_PAGINATED_REQUESTS {
            cursor.request(MAX_RPC_PAGE_SIZE).unwrap();
            cursor.advance(true, next).unwrap();
        }

        let error = cursor.request(MAX_RPC_PAGE_SIZE).unwrap_err();

        assert!(error.to_string().contains("pagination exceeded"));
    }
}
