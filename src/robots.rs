use crate::error::UnexpectedStatusError;
use anyhow::Result;
use reqwest::header::USER_AGENT;
use robotstxt::matcher::{LongestMatchRobotsMatchStrategy, RobotsMatchStrategy};
use robotstxt::{get_path_params_query, parse_robotstxt, RobotsParseHandler};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// The handler that parses the `robots.txt` into a `RobotsData`
#[derive(Debug, Clone, Default)]
pub struct RobotsHandler {
    // list of groups with all their user_agents
    groups: Vec<(HashSet<String>, Group)>,
    /// the current group that is processed
    group: Option<Group>,
    /// the list of agents for the current group that is currently processed
    agents: Option<HashSet<String>>,
}

impl RobotsHandler {
    pub async fn from_response(resp: reqwest::Response) -> Result<RobotsData> {
        let status_code = resp.status().as_u16();

        if (200..300).contains(&status_code) {
            let dbg_headers = resp.headers();
            dbg!("robots");
            dbg!(dbg_headers);
            let txt = resp.text_with_charset("").await?;
            let mut handler = RobotsHandler::default();
            parse_robotstxt(&txt, &mut handler);
            return Ok(handler.finish());
        }

        // See https://developers.google.com/webmasters/control-crawl-index/docs/robots_txt
        //
        // Google treats all 4xx errors in the same way and assumes that no valid
        // robots.txt file exists. It is assumed that there are no restrictions.
        // This is a "full allow" for crawling. Note: this includes 401
        // "Unauthorized" and 403 "Forbidden" HTTP result codes.
        if (400..500).contains(&status_code) {
            return Ok(RobotsData::allow_all());
        }

        // From Google's spec:
        // Server errors (5xx) are seen as temporary errors that result in a "full
        // disallow" of crawling.
        if (500..600).contains(&status_code) {
            return Ok(RobotsData::disallow_all());
        }

        // status code not recognized
        Err(UnexpectedStatusError::new(status_code).into())
    }

    fn finish_group(&mut self) {
        if let Some(group) = self.group.take() {
            self.groups
                .push((self.agents.take().unwrap_or_default(), group));
        }
    }

    pub fn finish(self) -> RobotsData {
        let mut groups = Vec::with_capacity(self.groups.len());
        let mut group_agents =
            HashMap::with_capacity(self.groups.iter().map(|(a, _)| a.len()).sum());
        for (idx, (agents, group)) in self.groups.into_iter().enumerate() {
            for agent in agents {
                let agent_groups = group_agents
                    .entry(agent)
                    .or_insert_with(|| Vec::with_capacity(1));
                agent_groups.push(idx);
            }
            groups.push(group)
        }

        RobotsData {
            groups,
            group_agents,
            allow_all: false,
            disallow_all: false,
        }
    }
}

impl RobotsParseHandler for RobotsHandler {
    fn handle_robots_start(&mut self) {}

    fn handle_robots_end(&mut self) {
        self.finish_group();
    }

    fn handle_user_agent(&mut self, _: u32, user_agent: &str) {
        self.finish_group();
        self.agents
            .get_or_insert(HashSet::with_capacity(1))
            .insert(user_agent.to_string());
    }

    fn handle_allow(&mut self, _: u32, value: &str) {
        if !value.is_empty() {
            let group = self.group.get_or_insert(Group::default());
            group.rules.push(Rule::allow(value));
        }
    }

    fn handle_disallow(&mut self, _: u32, value: &str) {
        if !value.is_empty() {
            let group = self.group.get_or_insert(Group::default());
            group.rules.push(Rule::disallow(value));
        }
    }

    fn handle_sitemap(&mut self, _: u32, _: &str) {}

    fn handle_unknown_action(&mut self, _: u32, action: &str, value: &str) {
        if let Some(group) = self.group.as_mut() {
            match action.to_lowercase().as_str() {
                "crawldelay" | "crawl-delay" => {
                    if let Ok(sec) = value.parse::<u64>() {
                        group.crawl_delay = Some(Duration::from_secs(sec % 1000));
                    } else if let Ok(sec) = value.parse::<f64>() {
                        group.crawl_delay = Some(Duration::from_millis((sec * 1000.) as u64));
                    }
                }
                _ => {}
            }
        }
    }
}

/// Represents a `robots.txt` file
#[derive(Debug, Clone)]
pub struct RobotsData {
    /// All the groups in the `robots.txt`
    pub groups: Vec<Group>,
    /// Mapping of all user-agents to all their groups
    pub group_agents: HashMap<String, Vec<usize>>,
    /// Whether to allow all url patterns for all user agents
    pub allow_all: bool,
    /// disallow all url patterns for all user agents
    pub disallow_all: bool,
}

impl RobotsData {
    /// All patterns for all agents are allowed
    pub fn allow_all() -> Self {
        Self {
            groups: Default::default(),
            group_agents: Default::default(),
            allow_all: true,
            disallow_all: false,
        }
    }

    /// All patterns for all agents are disallowed
    pub fn disallow_all() -> Self {
        Self {
            groups: Default::default(),
            group_agents: Default::default(),
            allow_all: false,
            disallow_all: true,
        }
    }

    /// Validate that the requested url is *NOT* disallowed.
    ///
    /// This does *NOT* mean that it is explicitly allowed
    pub fn is_not_disallowed(&self, request: &reqwest::Request) -> bool {
        if self.disallow_all {
            return false;
        }
        if self.allow_all {
            return true;
        }

        let path = get_path_params_query(request.url().path());

        // if the requests user-agent is not contained in the robots agent list, use the
        // wildcard instead
        let agent = request
            .headers()
            .get(USER_AGENT)
            .and_then(|agent| agent.to_str().ok())
            .unwrap_or("*");

        // check if a rule explicitly disallows access, in other words: if there exists
        // a rule that disallows access for the url's path, then `is_disallowed == true`
        let is_disallowed = self
            .group_agents
            .get(agent)
            .map(|groups| {
                groups
                    .iter()
                    .copied()
                    .map(|i| &self.groups[i])
                    .flat_map(|g| g.rules.iter())
                    .filter(|rule| rule.is_disallow())
                    .any(|rule| LongestMatchRobotsMatchStrategy::matches(&path, &rule.pattern))
            })
            .unwrap_or_default();

        !is_disallowed
    }
}

/// A Set of rules for a list of user-agents
#[derive(Debug, Clone, Default)]
pub struct Group {
    /// See [Non standard extension](http://en.wikipedia.org/wiki/Robots_exclusion_standard#Nonstandard_extensions)
    /// Interpreted as seconds
    crawl_delay: Option<Duration>,
    /// applied
    rules: Vec<Rule>,
}

/// A rule that either allows or disallows an url pattern
#[derive(Debug, Clone)]
pub struct Rule {
    /// The url pattern like `/`
    pattern: String,
    /// Whether the `pattern` is allowed or disallowed
    allow: bool,
}

impl Rule {
    /// Whether the pattern allows accessing the url
    pub fn is_allow(&self) -> bool {
        self.allow
    }

    /// Whether the pattern disallows accessing the url
    pub fn is_disallow(&self) -> bool {
        !self.allow
    }

    /// Create a new rule to allow this `pattern`
    pub fn allow(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            allow: true,
        }
    }

    /// Create a new rule to disallow this `pattern`
    pub fn disallow(path: impl Into<String>) -> Self {
        Self {
            pattern: path.into(),
            allow: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn robots_disallow_all() {
        let mut handler = RobotsHandler::default();
        parse_robotstxt(
            "User-Agent: *
Disallow: /",
            &mut handler,
        );
        let data = handler.finish();
        assert_eq!(data.groups.len(), 1);

        let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build();
        let request = client
            .request(reqwest::Method::GET, "https://old.reddit.com/r/rust")
            .build()
            .unwrap();
        assert!(!data.is_not_disallowed(&request))
    }

    #[test]
    fn empty_robots() {
        let mut handler = RobotsHandler::default();
        parse_robotstxt("", &mut handler);
        let data = handler.finish();

        let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build();
        let request = client
            .request(reqwest::Method::GET, "https://old.reddit.com/r/rust")
            .build()
            .unwrap();
        assert!(data.is_not_disallowed(&request))
    }

    #[test]
    fn robots_path_rule() {
        let mut handler = RobotsHandler::default();
        parse_robotstxt(
            "User-Agent: *
Disallow: /r/rust",
            &mut handler,
        );
        let data = handler.finish();

        let client = reqwest_middleware::ClientBuilder::new(reqwest::Client::new()).build();
        let request = client
            .request(reqwest::Method::GET, "https://old.reddit.com/r/crust")
            .build()
            .unwrap();
        assert!(data.is_not_disallowed(&request))
    }
}
