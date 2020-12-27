use anyhow::Result;
use voyager::{Crawler, Response, Scraper};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    pub struct Reddit;

    impl Scraper for Reddit {
        type Output = String;
        type State = ();

        fn scrape(
            &mut self,
            response: Response<Self::State>,
            crawler: &mut Crawler<Self>,
        ) -> Result<Option<Self::Output>> {
            crawler.complete(move |client| async move {
                dbg!(response.state);
                client.get("").send().await?;
                Ok(Some("".to_string()))
            });

            Ok(None)
        }
    }

    Ok(())
}
