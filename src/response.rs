use reqwest::header::HeaderMap;
use reqwest::{StatusCode, Url};
use scraper::Html;

/// A successful response for an issued request
pub struct Response<T> {
    /// The depth of the request that was issued for this
    pub depth: usize,
    /// The url of the request that was issued.
    pub request_url: Url,
    /// The url of the response as received
    pub response_url: Url,
    /// The status code of the response
    pub response_status: StatusCode,
    /// The headers of the received response
    pub response_headers: HeaderMap,
    /// The full response text.
    pub text: String,
    /// The attached state of the scraper
    pub state: Option<T>,
}

impl<T> Response<T> {
    /// Returns the parsed Html document
    pub fn html(&self) -> Html {
        Html::parse_document(&self.text)
    }
}
