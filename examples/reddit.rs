use anyhow::Result;
use futures::StreamExt;
use reqwest::Url;
use voyager::scraper::Selector;
use voyager::{Collector, Crawler, CrawlerConfig, Response, Scraper};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    pub struct Reddit {
        post_selector: Selector,
        title_selector: Selector,
        base_url: Url,
    }

    impl Default for Reddit {
        fn default() -> Self {
            Self {
                post_selector: Selector::parse("div#siteTable div.thing").unwrap(),
                title_selector: Selector::parse("a.title").unwrap(),
                base_url: Url::parse("https://old.reddit.com").unwrap(),
            }
        }
    }

    #[derive(Debug)]
    pub enum RedditState {
        SubReddit { after: Option<String>, name: String },
        Post(Post),
    }

    #[derive(Debug)]
    pub struct Post {
        data_url: Url,
        subreddit: String,
        title: String,
        updoots: usize,
        position: usize,
        comments: Vec<String>,
        data_full_name: String,
    }

    impl Scraper for Reddit {
        type Output = Post;
        type State = RedditState;

        fn scrape(
            &mut self,
            response: Response<Self::State>,
            crawler: &mut Crawler<Self>,
        ) -> Result<Option<Self::Output>> {
            let html = response.html();

            if let Some(state) = response.state {
                match state {
                    RedditState::SubReddit { name, .. } => {
                        for (idx, el) in html.select(&self.post_selector).enumerate() {
                            let val = el.value();
                            let comments_count = val
                                .attr("data-comments-count")
                                .and_then(|s| s.parse::<usize>().ok())
                                .unwrap();

                            let entry_id = val.attr("data-fullname");

                            let post = Post {
                                data_url: val
                                    .attr("data-url")
                                    .and_then(|url| self.base_url.join(url).ok())
                                    .unwrap(),
                                subreddit: name.clone(),
                                title: el
                                    .select(&self.title_selector)
                                    .next()
                                    .map(|a| a.inner_html())
                                    .unwrap(),
                                updoots: val
                                    .attr("data-score")
                                    .and_then(|s| s.parse::<usize>().ok())
                                    .unwrap_or_default(),
                                position: idx + 1,
                                comments: Vec::with_capacity(comments_count),
                                data_full_name: entry_id.clone().map(|s| s.to_string()).unwrap(),
                            };

                            if comments_count > 0 {
                                let post_url = val
                                    .attr("data-permalink")
                                    .and_then(|url| self.base_url.join(url).ok())
                                    .unwrap();
                                crawler.visit_with_state(post_url, RedditState::Post(post));
                            } else {
                                return Ok(Some(post));
                            }
                        }
                    }
                    RedditState::Post(post) => {
                        // scrape comments..

                        return Ok(Some(post));
                    }
                }
            }
            Ok(None)
        }
    }

    let config = CrawlerConfig::default().allow_domain("old.reddit.com");
    let mut collector = Collector::new(Reddit::default(), config);

    collector.crawler_mut().visit_with_state(
        "https://old.reddit.com/r/rust/",
        RedditState::SubReddit {
            after: None,
            name: "rust".to_string(),
        },
    );

    while let Some(output) = collector.next().await {
        let post = output?;
        dbg!(post);
    }

    Ok(())
}
