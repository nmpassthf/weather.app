use std::collections::HashSet;

use anyhow::{Result, bail};

use crate::actor::{ProviderCity, ProviderProvince};

pub fn validate_provider_province_catalog(provinces: &[ProviderProvince]) -> Result<()> {
    validate_unique_codes(
        provinces
            .iter()
            .map(|province| province.provider_code.as_str()),
        "province",
    )
}

pub fn validate_provider_city_catalog(
    provider_province_code: &str,
    cities: &[ProviderCity],
) -> Result<()> {
    if provider_province_code.trim().is_empty() {
        bail!("provider province code must not be empty");
    }
    validate_unique_codes(
        cities.iter().map(|city| city.provider_code.as_str()),
        "city",
    )?;
    for city in cities {
        if city.provider_province_code != provider_province_code {
            bail!(
                "city `{}` belongs to provider province `{}`, expected `{provider_province_code}`",
                city.provider_code,
                city.provider_province_code
            );
        }
    }
    Ok(())
}

fn validate_unique_codes<'a>(codes: impl Iterator<Item = &'a str>, kind: &str) -> Result<()> {
    let mut seen = HashSet::new();
    for code in codes {
        if code.trim().is_empty() {
            bail!("provider {kind} code must not be empty");
        }
        if !seen.insert(code) {
            bail!("duplicate provider {kind} code `{code}`");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_catalog_identity_before_persistence() {
        assert!(validate_provider_city_catalog("", &[]).is_err());

        let empty_city_code = vec![ProviderCity {
            provider_code: " ".to_string(),
            provider_province_code: "EXPECTED".to_string(),
            province: String::new(),
            city: String::new(),
            url: String::new(),
        }];
        assert!(validate_provider_city_catalog("EXPECTED", &empty_city_code).is_err());

        let duplicate = vec![
            ProviderProvince {
                provider_code: "A".to_string(),
                name: "first".to_string(),
                url: String::new(),
            },
            ProviderProvince {
                provider_code: "A".to_string(),
                name: "second".to_string(),
                url: String::new(),
            },
        ];
        assert!(validate_provider_province_catalog(&duplicate).is_err());

        let wrong_scope = vec![ProviderCity {
            provider_code: "C".to_string(),
            provider_province_code: "OTHER".to_string(),
            province: String::new(),
            city: String::new(),
            url: String::new(),
        }];
        assert!(validate_provider_city_catalog("EXPECTED", &wrong_scope).is_err());
    }
}
