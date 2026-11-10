use adblock::{Engine, FilterSet, lists::ParseOptions, request::Request};

const BLOCKED_AD_HOSTS: &[&str] = &[
    "2mdn.net",
    "33across.com",
    "adform.net",
    "adition.com",
    "adnxs.com",
    "adroll.com",
    "adsafeprotected.com",
    "adsrvr.org",
    "adtech.de",
    "advertising.com",
    "amazon-adsystem.com",
    "appnexus.com",
    "bidswitch.net",
    "casalemedia.com",
    "criteo.com",
    "criteo.net",
    "doubleclick.net",
    "exoclick.com",
    "googleads.g.doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "imasdk.googleapis.com",
    "lijit.com",
    "media.net",
    "moatads.com",
    "openx.net",
    "outbrain.com",
    "pubmatic.com",
    "revcontent.com",
    "rubiconproject.com",
    "scorecardresearch.com",
    "smartadserver.com",
    "taboola.com",
    "teads.tv",
    "tradedoubler.com",
    "trafficjunky.com",
    "yieldlab.net",
    "yieldmo.com",
    "zedo.com",
];

pub(crate) fn should_block_url(value: &str) -> bool {
    let Some(authority) = value
        .split_once("://")
        .map(|(_, remainder)| remainder.split('/').next().unwrap_or_default())
    else {
        return false;
    };
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
        .split(':')
        .next()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    BLOCKED_AD_HOSTS
        .iter()
        .any(|rule| host == *rule || host.ends_with(&format!(".{rule}")))
}

pub(crate) struct NativeAdBlock {
    engine: Engine,
}

impl NativeAdBlock {
    pub(crate) fn new() -> Self {
        let mut filters = FilterSet::new(false);
        for list in [
            include_str!(
                "../../../../Extensions/uBlockOrigin/uBlock0.chromium/assets/thirdparties/easylist/easylist.txt"
            ),
            include_str!(
                "../../../../Extensions/uBlockOrigin/uBlock0.chromium/assets/thirdparties/easylist/easyprivacy.txt"
            ),
            include_str!(
                "../../../../Extensions/uBlockOrigin/uBlock0.chromium/assets/ublock/filters.min.txt"
            ),
        ] {
            filters.add_filter_list(list.to_owned(), ParseOptions::default());
        }
        Self {
            engine: Engine::new_with_filter_set(filters),
        }
    }

    pub(crate) fn should_block(&self, url: &str, source: &str, resource_type: &str) -> bool {
        should_block_url(url)
            || Request::new(url, source, resource_type, "get")
                .is_ok_and(|request| self.engine.check_network_request(&request).should_block())
    }
}

pub(crate) fn initialization_script() -> String {
    let hosts = serde_json::to_string(BLOCKED_AD_HOSTS).unwrap_or_else(|_| "[]".into());
    format!(
        "window.__zinkBlockedAdHosts={hosts};{}",
        include_str!("bin/zink-connect-adblock.js")
    )
}

#[cfg(test)]
mod tests {
    use super::{NativeAdBlock, should_block_url};

    #[test]
    fn blocks_ad_hosts_without_blocking_lookalikes() {
        assert!(should_block_url(
            "https://pagead2.googlesyndication.com/pagead/js/ads.js"
        ));
        assert!(should_block_url("https://ads.doubleclick.net/video"));
        assert!(!should_block_url("https://example.com/video"));
        assert!(!should_block_url("https://notdoubleclick.net/video"));
    }

    #[test]
    fn easylist_blocks_path_rules() {
        let engine = NativeAdBlock::new();
        assert!(engine.should_block(
            "https://demand.supply/banner.js",
            "https://example.com/article",
            "script"
        ));
    }
}
