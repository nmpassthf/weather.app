#[derive(Debug, Clone)]
pub struct ProviderProvince {
    pub provider_code: String,
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct ProviderCity {
    pub provider_code: String,
    pub provider_province_code: String,
    pub province: String,
    pub city: String,
    pub url: String,
}
