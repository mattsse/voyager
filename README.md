voyager
=========================

[<img alt="github" src="https://img.shields.io/badge/github-mattsse/voyager-8da0cb?style=for-the-badge&labelColor=555555&logo=github" height="20">](https://github.com/mattsse/voyager)
[<img alt="crates.io" src="https://img.shields.io/crates/v/voyager.svg?style=for-the-badge&color=fc8d62&logo=rust" height="20">](https://crates.io/crates/voyager)
[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-voyager-66c2a5?style=for-the-badge&labelColor=555555&logoColor=white&logo=data:image/svg+xml;base64,PHN2ZyByb2xlPSJpbWciIHhtbG5zPSJodHRwOi8vd3d3LnczLm9yZy8yMDAwL3N2ZyIgdmlld0JveD0iMCAwIDUxMiA1MTIiPjxwYXRoIGZpbGw9IiNmNWY1ZjUiIGQ9Ik00ODguNiAyNTAuMkwzOTIgMjE0VjEwNS41YzAtMTUtOS4zLTI4LjQtMjMuNC0zMy43bC0xMDAtMzcuNWMtOC4xLTMuMS0xNy4xLTMuMS0yNS4zIDBsLTEwMCAzNy41Yy0xNC4xIDUuMy0yMy40IDE4LjctMjMuNCAzMy43VjIxNGwtOTYuNiAzNi4yQzkuMyAyNTUuNSAwIDI2OC45IDAgMjgzLjlWMzk0YzAgMTMuNiA3LjcgMjYuMSAxOS45IDMyLjJsMTAwIDUwYzEwLjEgNS4xIDIyLjEgNS4xIDMyLjIgMGwxMDMuOS01MiAxMDMuOSA1MmMxMC4xIDUuMSAyMi4xIDUuMSAzMi4yIDBsMTAwLTUwYzEyLjItNi4xIDE5LjktMTguNiAxOS45LTMyLjJWMjgzLjljMC0xNS05LjMtMjguNC0yMy40LTMzLjd6TTM1OCAyMTQuOGwtODUgMzEuOXYtNjguMmw4NS0zN3Y3My4zek0xNTQgMTA0LjFsMTAyLTM4LjIgMTAyIDM4LjJ2LjZsLTEwMiA0MS40LTEwMi00MS40di0uNnptODQgMjkxLjFsLTg1IDQyLjV2LTc5LjFsODUtMzguOHY3NS40em0wLTExMmwtMTAyIDQxLjQtMTAyLTQxLjR2LS42bDEwMi0zOC4yIDEwMiAzOC4ydi42em0yNDAgMTEybC04NSA0Mi41di03OS4xbDg1LTM4Ljh2NzUuNHptMC0xMTJsLTEwMiA0MS40LTEwMi00MS40di0uNmwxMDItMzguMiAxMDIgMzguMnYuNnoiPjwvcGF0aD48L3N2Zz4K" height="20">](https://docs.rs/voyager)
[<img alt="build status" src="https://img.shields.io/github/workflow/status/mattsse/voyager/CI/main?style=for-the-badge" height="20">](https://github.com/mattsse/voyager/actions?query=branch%3Amain)

With voyager you can easily extract structured data from websites.

Write your own crawler/scraper with voyager following a state machine model.

## Example

The examples use [tokio](https://tokio.rs/) as its runtime, so your `Cargo.toml` could look like this:

```toml
[dependencies]
voyager = { version = "0.1" }
tokio = { version = "1.0", features = ["full"] }
```

### Declare your own Scraper and model

```rust
// Declare your scraper, with all the selectors etc.
struct HackernewsScraper {
    post_selector: Selector,
    author_selector: Selector,
    title_selector: Selector,
    comment_selector: Selector,
    max_page: usize,
}

/// The state model
#[derive(Debug)]
enum HackernewsState {
    Page(usize),
    Post,
}

/// The ouput the scraper should eventually produce
#[derive(Debug)]
struct Entry {
    author: String,
    url: Url,
    link: Option<String>,
    title: String,
}
```

### Implement the `voyager::Scraper` trait

A `Scraper` consists of two associated types:

* `Output`, the type the scraper eventually produces
* `State`, the type, the scraper can drag along several requests that eventually lead to an `Output`

and the `scrape` callback, which is invoked after each received response.

Based on the state attached to `response` you can supply the crawler with new urls to visit with, or without a state attached to it.

Scraping is done with [causal-agent/scraper](https://github.com/causal-agent/scraper).

```rust
impl Scraper for HackernewsScraper {
    type Output = Entry;
    type State = HackernewsState;

    /// do your scraping
    fn scrape(
        &mut self,
        response: Response<Self::State>,
        crawler: &mut Crawler<Self>,
    ) -> Result<Option<Self::Output>> {
        let html = response.html();

        if let Some(state) = response.state {
            match state {
                HackernewsState::Page(page) => {
                    // find all entries
                    for id in html
                        .select(&self.post_selector)
                        .filter_map(|el| el.value().attr("id"))
                    {
                        // submit an url to a post
                        crawler.visit_with_state(
                            &format!("https://news.ycombinator.com/item?id={}", id),
                            HackernewsState::Post,
                        );
                    }
                    if page < self.max_page {
                        // queue in next page
                        crawler.visit_with_state(
                            &format!("https://news.ycombinator.com/news?p={}", page + 1),
                            HackernewsState::Page(page + 1),
                        );
                    }
                }

                HackernewsState::Post => {
                    // scrape the entry
                    let entry = Entry {
                        // ...
                    };
                    return Ok(Some(entry))
                }
            }
        }

        Ok(None)
    }
}
```

### Setup and collect all the output

Configure the crawler with via `CrawlerConfig`:

* Allow/Block list of Domains
* Delays between requests
* Whether to respect the `Robots.txt` rules

Feed your config and an instance of your scraper to the `Collector` that drives the `Crawler` and forwards the responses to your `Scraper`.

```rust
use voyager::scraper::Selector;
use voyager::*;
use tokio::stream::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    
    // only fulfill requests to `news.ycombinator.com`
    let config = CrawlerConfig::default().allow_domain_with_delay(
        "news.ycombinator.com",
        // add a delay between requests
        RequestDelay::Fixed(std::time::Duration::from_millis(2_000)),
    );
    
    let mut collector = Collector::new(HackernewsScraper::default(), config);

    collector.crawler_mut().visit_with_state(
        "https://news.ycombinator.com/news",
        HackernewsState::Page(1),
    );

    while let Some(output) = collector.next().await {
        let post = output?;
        dbg!(post);
    }
    
    Ok(())
}
```

See [examples](./examples) for more.

### Inject async calls

Sometimes it might be helpful to execute some other calls first, get a token etc.,
You submit `async` closures to the crawler to manually get a response and inject a state or drive a state to completion

```rust

fn scrape(
    &mut self,
    response: Response<Self::State>,
    crawler: &mut Crawler<Self>,
) -> Result<Option<Self::Output>> {

    // inject your custom crawl function that produces a `reqwest::Response` and `Self::State` which will get passed to `scrape` when resolved.
    crawler.crawl(move |client| async move {
        let state = response.state;
        let auth = client.post("some auth end point ").send()?.await?.json().await?;
        // do other async tasks etc..
        let new_resp = client.get("the next html page").send().await?;
        Ok((new_resp, state))
    });
    
    // submit a crawling job that completes to `Self::Output` directly
    crawler.complete(move |client| async move {
        // do other async tasks to create a `Self::Output` instance
        let output = Self::Output{/*..*/};
        Ok(Some(output))
    });
    
    Ok(None)
}
```

### Recover a state that got lost

If the crawler encountered an error, due to a failed or disallowed http request, the error is reported as `CrawlError`, which carries the last valid state. The error then can be down casted.


```rust

let mut collector = Collector::new(HackernewsScraper::default(), config);

while let Some(output) = collector.next().await {
  match output {
    Ok(post) => {/**/}
    Err(err) => {
      // recover the state by downcasting the error
      if let Ok(err) = err.downcast::<CrawlError<<HackernewsScraper as Scraper>::State>>() {
        let last_state = err.state();
      }
    }
  }
}
```

Licensed under either of these:

* Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or
  https://www.apache.org/licenses/LICENSE-2.0)
* MIT license ([LICENSE-MIT](LICENSE-MIT) or
  https://opensource.org/licenses/MIT)
