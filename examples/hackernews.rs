#![allow(dead_code)]

use anyhow::Result;
use futures::StreamExt;
use reqwest::Url;
use std::time::Duration;
use voyager::scraper::Selector;
use voyager::{Collector, Crawler, CrawlerConfig, RequestDelay, Response, Scraper};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    struct HackernewsScraper {
        post_selector: Selector,
        author_selector: Selector,
        title_selector: Selector,
        comment_selector: Selector,
        base_url: Url,
        max_page: usize,
    }

    impl Default for HackernewsScraper {
        fn default() -> Self {
            Self {
                post_selector: Selector::parse("table.itemlist tr.athing").unwrap(),
                author_selector: Selector::parse("a.hnuser").unwrap(),
                title_selector: Selector::parse("td.title a").unwrap(),
                comment_selector: Selector::parse(".comment-tree tr.athing").unwrap(),
                base_url: Url::parse("https://news.ycombinator.com/").unwrap(),
                max_page: 1,
            }
        }
    }

    #[derive(Debug)]
    enum HackernewsState {
        Page(usize),
        Post,
    }

    #[derive(Debug)]
    struct Entry {
        author: String,
        url: Url,
        link: Option<String>,
        title: String,
        replies: Vec<Reply>,
    }

    #[derive(Debug)]
    struct Reply {
        author: String,
        url: Url,
        comment: String,
        replies: Vec<Reply>,
    }

    impl Scraper for HackernewsScraper {
        type Output = Entry;
        type State = HackernewsState;

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
                        let el_title = html.select(&self.title_selector).next().unwrap();
                        let author = html
                            .select(&self.author_selector)
                            .map(|el| el.inner_html())
                            .next();
                        if author.is_none() {
                            // promo
                            return Ok(None);
                        }
                        // scrape the post
                        let entry = Entry {
                            author: author.unwrap(),
                            url: response.response_url,
                            link: el_title.value().attr("href").map(str::to_string),
                            title: el_title.inner_html(),
                            replies: Vec::new(),
                        };

                        // scrape replies...

                        return Ok(Some(entry));
                    }
                }
            }

            Ok(None)
        }
    }

    let config = CrawlerConfig::default().allow_domain_with_delay(
        "news.ycombinator.com",
        RequestDelay::Fixed(Duration::from_millis(2_000)),
    );
    let mut collector = Collector::new(HackernewsScraper::default(), config);

    collector.crawler_mut().visit_with_state(
        "https://news.ycombinator.com/news",
        HackernewsState::Page(1),
    );

    while let Some(output) = collector.next().await {
        let post = output;
        dbg!(post);
    }

    Ok(())
}
