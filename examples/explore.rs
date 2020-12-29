use anyhow::Result;
use futures::StreamExt;
use reqwest::Url;
use std::collections::{HashMap, HashSet};
use voyager::scraper::Selector;
use voyager::{Collector, Crawler, CrawlerConfig, Response, Scraper};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    struct Explorer {
        /// visited urls mapped with all the urls that link to that url
        visited: HashMap<Url, HashSet<Url>>,
        link_selector: Selector,
    }
    impl Default for Explorer {
        fn default() -> Self {
            Self {
                visited: Default::default(),
                link_selector: Selector::parse("a").unwrap(),
            }
        }
    }

    impl Scraper for Explorer {
        type Output = (usize, Url);
        type State = Url;

        fn scrape(
            &mut self,
            mut response: Response<Self::State>,
            crawler: &mut Crawler<Self>,
        ) -> Result<Option<Self::Output>> {
            if let Some(origin) = response.state.take() {
                self.visited
                    .entry(response.response_url.clone())
                    .or_default()
                    .insert(origin);
            }

            for link in response.html().select(&self.link_selector) {
                if let Some(href) = link.value().attr("href") {
                    if let Ok(url) = response.response_url.join(href) {
                        crawler.visit_with_state(url, response.response_url.clone());
                    }
                }
            }

            Ok(Some((response.depth, response.response_url)))
        }
    }

    let config = CrawlerConfig::default()
        .disallow_domains(vec!["facebook.com", "google.com"])
        // stop after 3 jumps
        .max_depth(4)
        // maximum of requests that are active
        .max_concurrent_requests(1_000);
    let mut collector = Collector::new(Explorer::default(), config);

    collector.crawler_mut().visit("https://www.wikipedia.org/");

    while let Some(output) = collector.next().await {
        if let Ok((depth, url)) = output {
            println!("Visited {} at depth: {}", url, depth);
        }
    }

    Ok(())
}
