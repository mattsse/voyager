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
        updoots_selector: Selector,
        current_page: usize,
    }

    impl Default for Reddit {
        fn default() -> Self {
            Self {
                post_selector: Selector::parse("div#siteTable div.thing").unwrap(),
                title_selector: Selector::parse("a").unwrap(),
                updoots_selector: Selector::parse("a").unwrap(),
                current_page: 0,
            }
        }
    }

    #[derive(Debug)]
    pub enum RedditState {
        Page,
        Post(Post),
    }

    #[derive(Debug)]
    pub struct Post {
        post_url: Url,
        subreddit: String,
        title: String,
        updoots: usize,
        position: usize,
        comments: Vec<String>,
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

            let posts = html
                .select(&self.post_selector)
                .filter_map(|el| el.value().attr("data-url"))
                .collect::<Vec<_>>();

            // if let Some(state) = response.state {
            //     match state {
            //         RedditState::Page => {
            //             // find all links and vist
            //         }
            //         RedditState::Post(position) => {
            //             // scrape and return
            //         }
            //     }
            // }

            Ok(None)
        }
    }

    let config = CrawlerConfig::default().allow_domain("old.reddit.com");
    let mut collector = Collector::new(Reddit::default(), config);

    collector
        .crawler_mut()
        .visit_with_state("https://old.reddit.com/r/rust/", RedditState::Page);

    while let Some(output) = collector.next().await {
        let post = output.unwrap();
        dbg!(post);
    }

    Ok(())
}
