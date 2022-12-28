//! Holds `Blocker`, which handles all network-based adblocking queries.

use once_cell::sync::Lazy;
use std::ops::DerefMut;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::collections::{HashMap, HashSet};

#[cfg(feature = "object-pooling")]
use lifeguard::Pool;

use crate::filters::network::{NetworkFilter, NetworkMatchable, RegexManager, RegexDebugEntry};
use crate::request::Request;
use crate::utils::{fast_hash, Hash};
use crate::optimizer;
use crate::resources::{Resource, RedirectResourceStorage, RedirectResource};
use crate::utils;

pub struct BlockerOptions {
    pub enable_optimizations: bool,
}

#[derive(Debug, Serialize)]
pub struct BlockerResult {
    pub matched: bool,
    /// Important is used to signal that a rule with the `important` option
    /// matched. An `important` match means that exceptions should not apply
    /// and no further checking is neccesary--the request should be blocked
    /// (empty body or cancelled).
    ///
    /// Brave Browser keeps seperate instances of [`Blocker`] for default
    /// lists and regional ones, so `important` here is used to correct
    /// behaviour between them: checking should stop instead of moving to the
    /// next instance iff an `important` rule matched.
    pub important: bool,
    /// Specifies what to load instead of the original request, rather than
    /// just blocking it outright. This can come from a filter with a `redirect`
    /// or `redirect-rule` option. If present, the field will contain the body
    /// of the redirect to be injected.
    ///
    /// Note that the presence of a redirect does _not_ imply that the request
    /// should be blocked. The `redirect-rule` option can produce a redirection
    /// that's only applied if another blocking filter matches a request.
    pub redirect: Option<String>,
    /// `removeparam` may remove URL parameters. If the original request URL was
    /// modified at all, the new version will be here. This should be used
    /// as long as the request is not blocked.
    pub rewritten_url: Option<String>,
    /// Exception is `Some` when the blocker matched on an exception rule.
    /// Effectively this means that there was a match, but the request should
    /// not be blocked. It is a non-empty string if the blocker was initialized
    /// from a list of rules with debugging enabled, otherwise the original
    /// string representation is discarded to reduce memory use.
    pub exception: Option<String>,
    /// Filter--similarly to exception--includes the string representation of
    /// the rule when there is a match and debugging is enabled. Otherwise, on
    /// a match, it is `Some`.
    pub filter: Option<String>,
    /// The `error` field is only used to signal that there was an error in
    /// parsing the provided URLs when using the simpler
    /// [`crate::engine::Engine::check_network_urls`] method.
    pub error: Option<String>,
}

impl Default for BlockerResult {
    fn default() -> BlockerResult {
        BlockerResult {
            matched: false,
            important: false,
            redirect: None,
            rewritten_url: None,
            exception: None,
            filter: None,
            error: None,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum BlockerError {
    SerializationError,
    DeserializationError,
    OptimizedFilterExistence,
    BadFilterAddUnsupported,
    FilterExists,
}

pub struct BlockerDebugInfo {
    pub regex_data: Vec<RegexDebugEntry>,
    pub compiled_regex_count: u64,
}

#[cfg(feature = "object-pooling")]
pub struct TokenPool {
    pub pool: Pool<Vec<utils::Hash>>
}

#[cfg(feature = "object-pooling")]
impl Default for TokenPool {
    fn default() -> TokenPool {
        TokenPool {
            pool: lifeguard::pool()
                .with(lifeguard::StartingSize(1))
                .with(lifeguard::Supplier(|| Vec::with_capacity(utils::TOKENS_BUFFER_SIZE)))
                .build()
        }
    }
}

// only check for tags in tagged and exception rule buckets,
// pass empty set for the rest
static NO_TAGS: Lazy<HashSet<String>> = Lazy::new(HashSet::new);

/// Stores network filters for efficient querying.
pub struct Blocker {
    pub(crate) csp: NetworkFilterList,
    pub(crate) exceptions: NetworkFilterList,
    pub(crate) importants: NetworkFilterList,
    pub(crate) redirects: NetworkFilterList,
    pub(crate) removeparam: NetworkFilterList,
    pub(crate) filters_tagged: NetworkFilterList,
    pub(crate) filters: NetworkFilterList,
    pub(crate) generic_hide: NetworkFilterList,

    // Enabled tags are not serialized - when deserializing, tags of the existing
    // instance (the one we are recreating lists into) are maintained
    pub(crate) tags_enabled: HashSet<String>,
    pub(crate) tagged_filters_all: Vec<NetworkFilter>,

    pub(crate) enable_optimizations: bool,

    pub(crate) resources: RedirectResourceStorage,
    // Not serialized
    #[cfg(feature = "object-pooling")]
    pub(crate) pool: TokenPool,

    // Not serialized
    #[cfg(feature = "unsync-regex-caching")]
    pub(crate) regex_manager: std::cell::RefCell<RegexManager>,
    #[cfg(not(feature = "unsync-regex-caching"))]
    pub(crate) regex_manager: std::sync::Mutex<RegexManager>,
}

impl Blocker {
    /// Decide if a network request (usually from WebRequest API) should be
    /// blocked, redirected or allowed.
    pub fn check(&self, request: &Request) -> BlockerResult {
        self.check_parameterised(request, false, false)
    }

    #[cfg(feature = "unsync-regex-caching")]
    fn borrow_regex_manager(&self) -> std::cell::RefMut<RegexManager> {
        let mut manager = self.regex_manager.borrow_mut();
        manager.update_time();
        manager
    }

    #[cfg(not(feature = "unsync-regex-caching"))]
    fn borrow_regex_manager(&self) -> std::sync::MutexGuard<RegexManager> {
        let mut manager = self.regex_manager.lock().unwrap();
        manager.update_time();
        manager
    }

    pub fn check_generic_hide(&self, hostname_request: &Request) -> bool {
        let mut regex_manager = self.borrow_regex_manager();
        let mut request_tokens;
        #[cfg(feature = "object-pooling")]
        {
            request_tokens = self.pool.pool.new();
        }
        #[cfg(not(feature = "object-pooling"))]
        {
            request_tokens = Vec::with_capacity(utils::TOKENS_BUFFER_SIZE);
        }
        hostname_request.get_tokens(&mut request_tokens);

        self.generic_hide
            .check(
                hostname_request,
                &request_tokens,
                &HashSet::new(),
                &mut regex_manager,
            )
            .is_some()
    }

    pub fn check_parameterised(
        &self,
        request: &Request,
        matched_rule: bool,
        force_check_exceptions: bool,
    ) -> BlockerResult {
        let mut regex_manager = self.borrow_regex_manager();
        if !request.is_supported {
            return BlockerResult::default();
        }

        let mut request_tokens;
        #[cfg(feature = "object-pooling")]
        {
            request_tokens = self.pool.pool.new();
        }
        #[cfg(not(feature = "object-pooling"))]
        {
            request_tokens = Vec::with_capacity(utils::TOKENS_BUFFER_SIZE);
        }
        request.get_tokens(&mut request_tokens);

        // Check the filters in the following order:
        // 1. $important (not subject to exceptions)
        // 2. redirection ($redirect=resource)
        // 3. normal filters - if no match by then
        // 4. exceptions - if any non-important match of forced

        #[cfg(feature = "metrics")]
        print!("importants\t");
        // Always check important filters
        let important_filter = self.importants.check(
            request,
            &request_tokens,
            &NO_TAGS,
            &mut regex_manager,
        );

        // only check the rest of the rules if not previously matched
        let filter = if important_filter.is_none() && !matched_rule {
            #[cfg(feature = "metrics")]
            print!("tagged\t");
            self.filters_tagged
                .check(
                    request,
                    &request_tokens,
                    &self.tags_enabled,
                    &mut regex_manager,
                )
                .or_else(|| {
                    #[cfg(feature = "metrics")]
                    print!("filters\t");
                    self.filters.check(
                        request,
                        &request_tokens,
                        &NO_TAGS,
                        &mut regex_manager,
                    )
                })
        } else {
            important_filter
        };

        let exception = match filter.as_ref() {
            // if no other rule matches, only check exceptions if forced to
            None if matched_rule || force_check_exceptions => {
                #[cfg(feature = "metrics")]
                print!("exceptions\t");
                self.exceptions.check(
                    request,
                    &request_tokens,
                    &self.tags_enabled,
                    &mut regex_manager,
                )
            }
            None => None,
            // If matched an important filter, exceptions don't atter
            Some(f) if f.is_important() => None,
            Some(_) => {
                #[cfg(feature = "metrics")]
                print!("exceptions\t");
                self.exceptions.check(
                    request,
                    &request_tokens,
                    &self.tags_enabled,
                    &mut regex_manager,
                )
            }
        };

        #[cfg(feature = "metrics")]
        println!();

        let redirect_filters = self.redirects.check_all(
            request,
            &request_tokens,
            &NO_TAGS,
            regex_manager.deref_mut(),
        );

        // Extract the highest priority redirect directive.
        // So far, priority specifiers are not supported, which means:
        // 1. Exceptions - can bail immediately if found
        // 2. Any other redirect resource
        let redirect_resource = {
            let mut exceptions = vec![];
            for redirect_filter in redirect_filters.iter() {
                if redirect_filter.is_exception() {
                    if let Some(redirect) = redirect_filter.modifier_option.as_ref() {
                        exceptions.push(redirect);
                    }
                }
            }
            let mut resource_and_priority = None;
            for redirect_filter in redirect_filters.iter() {
                if !redirect_filter.is_exception() {
                    if let Some(redirect) = redirect_filter.modifier_option.as_ref() {
                        if !exceptions.contains(&&redirect) {
                            // parse redirect + priority
                            let (resource, priority) = if let Some(idx) = redirect.rfind(':') {
                                let priority_str = &redirect[idx + 1..];
                                let resource = &redirect[..idx];
                                if let Ok(priority) = priority_str.parse::<i32>() {
                                    (resource, priority)
                                } else {
                                    (&redirect[..], 0)
                                }
                            } else {
                                (&redirect[..], 0)
                            };
                            if let Some((_, p1)) = resource_and_priority {
                                if priority > p1 {
                                    resource_and_priority = Some((resource, priority));
                                }
                            } else {
                                resource_and_priority = Some((resource, priority));
                            }
                        }
                    }
                }
            }
            resource_and_priority.map(|(r, _)| r)
        };

        let redirect: Option<String> = redirect_resource.and_then(|resource_name| {
            if let Some(resource) = self.resources.get_resource(resource_name) {
                // Only match resource redirects if a matching resource exists
                let data_url = format!("data:{};base64,{}", resource.content_type, &resource.data);
                Some(data_url.trim().to_owned())
            } else {
                // It's acceptable to pass no redirection if no matching resource is loaded.
                // TODO - it may be useful to return a status flag to indicate that this occurred.
                #[cfg(test)]
                eprintln!("Matched rule with redirect option but did not find corresponding resource to send");
                None
            }
        });

        let important = filter.is_some() && filter.as_ref().map(|f| f.is_important()).unwrap_or_else(|| false);

        let rewritten_url = if important {
            None
        } else {
            Self::apply_removeparam(
                &self.removeparam,
                request,
                request_tokens,
                regex_manager.deref_mut(),
            )
        };

        // If something has already matched before but we don't know what, still return a match
        let matched = exception.is_none() && (filter.is_some() || matched_rule);
        BlockerResult {
            matched,
            important,
            redirect,
            rewritten_url,
            exception: exception.as_ref().map(|f| f.to_string()), // copy the exception
            filter: filter.as_ref().map(|f| f.to_string()),       // copy the filter
            error: None,
        }
    }

    fn apply_removeparam(
        removeparam_filters: &NetworkFilterList,
        request: &Request,
        request_tokens: lifeguard::Recycled<Vec<u64>>,
        regex_manager: &mut RegexManager,
    ) -> Option<String> {
        /// Represents an `&`-separated argument from a URL query parameter string
        enum QParam<'a> {
            /// Just a key, e.g. `...&key&...`
            KeyOnly(&'a str),
            /// Key-value pair separated by an equal sign, e.g. `...&key=value&...`
            KeyValue(&'a str, &'a str),
        }

        impl<'a> std::fmt::Display for QParam<'a> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self {
                    Self::KeyOnly(k) => write!(f, "{}", k),
                    Self::KeyValue(k, v) => write!(f, "{}={}", k, v),
                }
            }
        }

        let url = &request.original_url;
        // Only check for removeparam if there's a query string in the request URL
        if let Some(i) = url.find('?') {
            // String indexing safety: indices come from `.len()` or `.find(..)` on individual
            // ASCII characters (1 byte each), some plus 1.
            let params_start = i + 1;
            let hash_index = if let Some(j) = url[params_start..].find('#') { params_start + j } else { url.len() };
            let qparams = &url[params_start..hash_index];
            let mut params: Vec<(QParam, bool)> = qparams
                .split('&')
                .map(|pair| {
                    if let Some((k, v)) = pair.split_once('=') {
                        QParam::KeyValue(k, v)
                    } else {
                        QParam::KeyOnly(pair)
                    }
                })
                .map(|param| (param, true))
                .collect();

            let filters = removeparam_filters.check_all(request, &request_tokens, &NO_TAGS, regex_manager);
            let mut rewrite = false;
            for removeparam_filter in filters {
                if let Some(removeparam) = &removeparam_filter.modifier_option {
                    params.iter_mut().for_each(|(param, include)| {
                        if let QParam::KeyValue(k, v) = param {
                            if !v.is_empty() && k == removeparam {
                                *include = false;
                                rewrite = true;
                            }
                        }
                    });
                }
            }
            if rewrite {
                let p = itertools::join(params.into_iter().filter(|(_, include)| *include).map(|(param, _)| param.to_string()), "&");
                let new_param_str = if p.is_empty() {
                    String::from("")
                } else {
                    format!("?{}", p)
                };
                Some(format!("{}{}{}", &url[0..i], new_param_str, &url[hash_index..]))
            } else {
                None
            }
        } else {
            None
        }
    }

    /// Given a "main_frame" or "subdocument" request, check if some content security policies
    /// should be injected in the page.
    pub fn get_csp_directives(&self, request: &Request) -> Option<String> {
        use crate::request::RequestType;

        if request.request_type != RequestType::Document && request.request_type != RequestType::Subdocument {
            return None;
        }

        let mut request_tokens;
        let mut regex_manager = self.borrow_regex_manager();

        #[cfg(feature = "object-pooling")]
        {
            request_tokens = self.pool.pool.new();
        }
        #[cfg(not(feature = "object-pooling"))]
        {
            request_tokens = Vec::with_capacity(utils::TOKENS_BUFFER_SIZE);
        }
        request.get_tokens(&mut request_tokens);

        let filters = self.csp.check_all(
            request,
            &request_tokens,
            &self.tags_enabled,
            &mut regex_manager,
        );

        if filters.is_empty() {
            return None;
        }

        let mut disabled_directives: HashSet<&str> = HashSet::new();
        let mut enabled_directives: HashSet<&str> = HashSet::new();

        for filter in filters {
            if filter.is_exception() {
                if filter.is_csp() {
                    if let Some(csp_directive) = &filter.modifier_option {
                        disabled_directives.insert(csp_directive);
                    } else {
                        // Exception filters with empty `csp` options will disable all CSP
                        // injections for matching pages.
                        return None
                    }
                }
            } else if filter.is_csp() {
                if let Some(csp_directive) = &filter.modifier_option {
                    enabled_directives.insert(csp_directive);
                }
            }
        }

        let mut remaining_directives = enabled_directives.difference(&disabled_directives);

        let mut merged = if let Some(directive) = remaining_directives.next() {
            String::from(*directive)
        } else {
            return None;
        };

        remaining_directives.for_each(|directive| {
            merged.push(',');
            merged.push_str(directive);
        });

        Some(merged)
    }

    pub fn new(network_filters: Vec<NetworkFilter>, options: &BlockerOptions) -> Blocker {
        // Capacity of filter subsets estimated based on counts in EasyList and EasyPrivacy - if necessary
        // the Vectors will grow beyond the pre-set capacity, but it is more efficient to allocate all at once
        // $csp=
        let mut csp = Vec::with_capacity(200);
        // @@filter
        let mut exceptions = Vec::with_capacity(network_filters.len() / 8);
        // $important
        let mut importants = Vec::with_capacity(200);
        // $redirect, $redirect-rule
        let mut redirects = Vec::with_capacity(200);
        // $removeparam
        let mut removeparam = Vec::with_capacity(60);
        // $tag=
        let mut tagged_filters_all = Vec::with_capacity(200);
        // $badfilter
        let mut badfilters = Vec::with_capacity(100);
        // $generichide
        let mut generic_hide = Vec::with_capacity(4000);
        // All other filters
        let mut filters = Vec::with_capacity(network_filters.len());

        // Injections
        // TODO: resource handling

        if !network_filters.is_empty() {
            for filter in network_filters.iter() {
                if filter.is_badfilter() {
                    badfilters.push(filter);
                }
            }
            let badfilter_ids: HashSet<Hash> = badfilters.iter().map(|f| f.get_id_without_badfilter()).collect();
            for filter in network_filters {
                // skip any bad filters
                let filter_id = filter.get_id();
                if badfilter_ids.contains(&filter_id) || filter.is_badfilter() {
                    continue;
                }

                // Redirects are independent of blocking behavior.
                if filter.is_redirect() {
                    redirects.push(filter.clone());
                }

                if filter.is_csp() {
                    csp.push(filter);
                } else if filter.is_removeparam() {
                    removeparam.push(filter);
                } else if filter.is_generic_hide() {
                    generic_hide.push(filter);
                } else if filter.is_exception() {
                    exceptions.push(filter);
                } else if filter.is_important() {
                    importants.push(filter);
                } else if filter.tag.is_some() && !filter.is_redirect() {
                    // `tag` + `redirect` is unsupported for now.
                    tagged_filters_all.push(filter);
                } else {
                    if (filter.is_redirect() && filter.also_block_redirect()) || !filter.is_redirect() {
                        filters.push(filter);
                    }
                }
            }
        }

        tagged_filters_all.shrink_to_fit();

        Blocker {
            csp: NetworkFilterList::new(csp, options.enable_optimizations),
            exceptions: NetworkFilterList::new(exceptions, options.enable_optimizations),
            importants: NetworkFilterList::new(importants, options.enable_optimizations),
            redirects: NetworkFilterList::new(redirects, options.enable_optimizations),
            removeparam: NetworkFilterList::new(removeparam, options.enable_optimizations),
            filters_tagged: NetworkFilterList::new(Vec::new(), options.enable_optimizations),
            filters: NetworkFilterList::new(filters, options.enable_optimizations),
            generic_hide: NetworkFilterList::new(generic_hide, options.enable_optimizations),
            // Tags special case for enabling/disabling them dynamically
            tags_enabled: HashSet::new(),
            tagged_filters_all,
            // Options
            enable_optimizations: options.enable_optimizations,

            resources: RedirectResourceStorage::default(),
            #[cfg(feature = "object-pooling")]
            pool: TokenPool::default(),
            regex_manager:Default::default(),
        }
    }

    /// If optimizations are enabled, the `Blocker` will be configured to automatically optimize
    /// its filters after batch updates. However, even if they are disabled, it is possible to
    /// manually call `optimize()`. It may be useful to have finer-grained control over
    /// optimization scheduling when frequently updating filters.
    pub fn optimize(&mut self) {
        self.csp.optimize();
        self.exceptions.optimize();
        self.importants.optimize();
        self.redirects.optimize();
        self.removeparam.optimize();
        self.filters_tagged.optimize();
        self.filters.optimize();
        self.generic_hide.optimize();
    }

    pub fn filter_exists(&self, filter: &NetworkFilter) -> bool {
        if filter.is_csp() {
            self.csp.filter_exists(filter)
        } else if filter.is_generic_hide() {
            self.generic_hide.filter_exists(filter)
        } else if filter.is_exception() {
            self.exceptions.filter_exists(filter)
        } else if filter.is_important() {
            self.importants.filter_exists(filter)
        } else if filter.is_redirect() {
            self.redirects.filter_exists(filter)
        } else if filter.is_removeparam() {
            self.removeparam.filter_exists(filter)
        } else if filter.tag.is_some() {
            self.tagged_filters_all.iter().any(|f| f.id == filter.id)
        } else {
            self.filters.filter_exists(filter)
        }
    }

    pub fn add_filter(&mut self, filter: NetworkFilter) -> Result<(), BlockerError> {
        // Redirects are independent of blocking behavior.
        if filter.is_redirect() {
            self.redirects.add_filter(filter.clone());
        }

        if filter.is_badfilter() {
            Err(BlockerError::BadFilterAddUnsupported)
        } else if self.filter_exists(&filter) {
            Err(BlockerError::FilterExists)
        } else if filter.is_csp() {
            self.csp.add_filter(filter);
            Ok(())
        } else if filter.is_generic_hide() {
            self.generic_hide.add_filter(filter);
            Ok(())
        } else if filter.is_exception() {
            self.exceptions.add_filter(filter);
            Ok(())
        } else if filter.is_important() {
            self.importants.add_filter(filter);
            Ok(())
        } else if filter.is_removeparam() {
            self.removeparam.add_filter(filter);
            Ok(())
        } else if filter.tag.is_some() && !filter.is_redirect() {
            // `tag` + `redirect` is unsupported
            self.tagged_filters_all.push(filter);
            let tags_enabled = self.tags_enabled().into_iter().collect::<HashSet<_>>();
            self.tags_with_set(tags_enabled);
            Ok(())
        } else if (filter.is_redirect() && filter.also_block_redirect()) || !filter.is_redirect() {
            self.filters.add_filter(filter);
            Ok(())
        } else {
            Ok(())
        }
    }

    pub fn use_tags(&mut self, tags: &[&str]) {
        let tag_set: HashSet<String> = tags.iter().map(|&t| String::from(t)).collect();
        self.tags_with_set(tag_set);
    }

    pub fn enable_tags(&mut self, tags: &[&str]) {
        let tag_set: HashSet<String> = tags.iter().map(|&t| String::from(t)).collect::<HashSet<_>>()
            .union(&self.tags_enabled)
            .cloned()
            .collect();
        self.tags_with_set(tag_set);
    }

    pub fn disable_tags(&mut self, tags: &[&str]) {
        let tag_set: HashSet<String> = self.tags_enabled
            .difference(&tags.iter().map(|&t| String::from(t)).collect())
            .cloned()
            .collect();
        self.tags_with_set(tag_set);
    }

    fn tags_with_set(&mut self, tags_enabled: HashSet<String>) {
        self.tags_enabled = tags_enabled;
        let filters: Vec<NetworkFilter> = self.tagged_filters_all.iter()
            .filter(|n| n.tag.is_some() && self.tags_enabled.contains(n.tag.as_ref().unwrap()))
            .cloned()
            .collect();
        self.filters_tagged = NetworkFilterList::new(filters, self.enable_optimizations);
    }

    pub fn tags_enabled(&self) -> Vec<String> {
        self.tags_enabled.iter().cloned().collect()
    }

    pub fn use_resources(&mut self, resources: &[Resource]) {
        let resources = RedirectResourceStorage::from_resources(resources);
        self.resources = resources;
    }

    pub fn add_resource(&mut self, resource: &Resource) -> Result<(), crate::resources::AddResourceError> {
        self.resources.add_resource(resource)
    }

    pub fn get_resource(&self, key: &str) -> Option<&RedirectResource> {
        self.resources.get_resource(key)
    }

    #[cfg(feature = "debug-info")]
    pub fn get_debug_info(&self) -> BlockerDebugInfo {
        let regex_manager = self.borrow_regex_manager();
        BlockerDebugInfo {
            regex_data: regex_manager.get_debug_regex_data(),
            compiled_regex_count: regex_manager.get_compiled_regex_count(),
        }
    }

}

#[derive(Serialize, Deserialize, Default)]
pub struct NetworkFilterList {
    #[serde(serialize_with = "crate::data_format::utils::stabilize_hashmap_serialization")]
    pub(crate) filter_map: HashMap<Hash, Vec<Arc<NetworkFilter>>>,
}

impl NetworkFilterList {
    pub fn new(filters: Vec<NetworkFilter>, optimize: bool) -> NetworkFilterList {
        // Compute tokens for all filters
        let filter_tokens: Vec<_> = filters
            .into_iter()
            .map(|filter| {
                let tokens = filter.get_tokens();
                (Arc::new(filter), tokens)
            })
            .collect();
        // compute the tokens' frequency histogram
        let (total_number_of_tokens, tokens_histogram) = token_histogram(&filter_tokens);

        // Build a HashMap of tokens to Network Filters (held through Arc, Atomic Reference Counter)
        let mut filter_map = HashMap::with_capacity(filter_tokens.len());
        {
            for (filter_pointer, multi_tokens) in filter_tokens {
                for tokens in multi_tokens {
                    let mut best_token: Hash = 0;
                    let mut min_count = total_number_of_tokens + 1;
                    for token in tokens {
                        match tokens_histogram.get(&token) {
                            None => {
                                min_count = 0;
                                best_token = token
                            }
                            Some(&count) if count < min_count => {
                                min_count = count;
                                best_token = token
                            }
                            _ => {}
                        }
                    }
                    insert_dup(&mut filter_map, best_token, Arc::clone(&filter_pointer));
                }
            }
        }

        let mut self_ = NetworkFilterList {
            filter_map,
        };

        if optimize {
            self_.optimize();
        } else {
            self_.filter_map.shrink_to_fit();
        }

        self_
    }

    pub fn optimize(&mut self) {
        let mut optimized_map = HashMap::with_capacity(self.filter_map.len());
        for (key, filters) in self.filter_map.drain() {
            let mut unoptimized: Vec<NetworkFilter> = Vec::with_capacity(filters.len());
            let mut unoptimizable: Vec<Arc<NetworkFilter>> = Vec::with_capacity(filters.len());
            for f in filters {
                match Arc::try_unwrap(f) {
                    Ok(f) => unoptimized.push(f),
                    Err(af) => unoptimizable.push(af)
                }
            }

            let mut optimized: Vec<_> = if unoptimized.len() > 1 {
                optimizer::optimize(unoptimized).into_iter().map(Arc::new).collect()
            } else {
                // nothing to optimize
                unoptimized.into_iter().map(Arc::new).collect()
            };

            optimized.append(&mut unoptimizable);
            optimized_map.insert(key, optimized);
        }

        // won't mutate anymore, shrink to fit items
        optimized_map.shrink_to_fit();

        self.filter_map = optimized_map;
    }

    pub fn add_filter(&mut self, filter: NetworkFilter) {
        let filter_tokens = filter.get_tokens();
        let total_rules = vec_hashmap_len(&self.filter_map);
        let filter_pointer = Arc::new(filter);

        for tokens in filter_tokens {
            let mut best_token: Hash = 0;
            let mut min_count = total_rules + 1;
            for token in tokens {
                match self.filter_map.get(&token) {
                    None => {
                        min_count = 0;
                        best_token = token
                    }
                    Some(filters) if filters.len() < min_count => {
                        min_count = filters.len();
                        best_token = token
                    }
                    _ => {}
                }
            }

            insert_dup(&mut self.filter_map, best_token, Arc::clone(&filter_pointer));
        }
    }

    pub fn filter_exists(&self, filter: &NetworkFilter) -> bool {
        // if self.optimized == Some(true) {
        //     return Err(BlockerError::OptimizedFilterExistence)
        // }
        let mut tokens: Vec<_> = filter.get_tokens().into_iter().flatten().collect();

        if tokens.is_empty() {
            tokens.push(0)
        }

        for token in tokens {
            if let Some(filters) = self.filter_map.get(&token) {
                for saved_filter in filters {
                    if saved_filter.id == filter.id {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Returns the first found filter, if any, that matches the given request. The backing storage
    /// has a non-deterministic order, so this should be used for any category of filters where a
    /// match from each would be functionally equivalent. For example, if two different exception
    /// filters match a certain request, it doesn't matter _which_ one is matched - the request
    /// will be excepted either way.
    pub fn check(
        &self,
        request: &Request,
        request_tokens: &[Hash],
        active_tags: &HashSet<String>,
        regex_manager: &mut RegexManager,
    ) -> Option<&NetworkFilter> {
        #[cfg(feature = "metrics")]
        let mut filters_checked = 0;
        #[cfg(feature = "metrics")]
        let mut filter_buckets = 0;

        #[cfg(not(feature = "metrics"))]
        {
            if self.filter_map.is_empty() {
                return None;
            }
        }

        if let Some(source_hostname_hashes) = request.source_hostname_hashes.as_ref() {
            for token in source_hostname_hashes {
                if let Some(filter_bucket) = self.filter_map.get(token) {
                    #[cfg(feature = "metrics")]
                    {
                        filter_buckets += 1;
                    }

                    for filter in filter_bucket {
                        #[cfg(feature = "metrics")]
                        {
                            filters_checked += 1;
                        }
                        // if matched, also needs to be tagged with an active tag (or not tagged at all)
                        if filter.matches(request, regex_manager)
                            && filter
                                .tag
                                .as_ref()
                                .map(|t| active_tags.contains(t))
                                .unwrap_or(true)
                        {
                            #[cfg(feature = "metrics")]
                            print!("true\t{}\t{}\tskipped\t{}\t{}\t", filter_buckets, filters_checked, filter_buckets, filters_checked);
                            return Some(filter);
                        }
                    }
                }
            }
        }

        #[cfg(feature = "metrics")]
        print!("false\t{}\t{}\t", filter_buckets, filters_checked);

        for token in request_tokens {
            if let Some(filter_bucket) = self.filter_map.get(token) {
                #[cfg(feature = "metrics")]
                {
                    filter_buckets += 1;
                }
                for filter in filter_bucket {
                    #[cfg(feature = "metrics")]
                    {
                        filters_checked += 1;
                    }
                    // if matched, also needs to be tagged with an active tag (or not tagged at all)
                    if filter.matches(request, regex_manager) && filter.tag.as_ref().map(|t| active_tags.contains(t)).unwrap_or(true) {
                        #[cfg(feature = "metrics")]
                        print!("true\t{}\t{}\t", filter_buckets, filters_checked);
                        return Some(filter);
                    }
                }
            }
        }

        #[cfg(feature = "metrics")]
        print!("false\t{}\t{}\t", filter_buckets, filters_checked);

        None
    }

    /// Returns _all_ filters that match the given request. This should be used for any category of
    /// filters where a match from each may carry unique information. For example, if two different
    /// `$csp` filters match a certain request, they may each carry a distinct CSP directive, and
    /// each directive should be combined for the final result.
    pub fn check_all(
        &self,
        request: &Request,
        request_tokens: &[Hash],
        active_tags: &HashSet<String>,
        regex_manager: &mut RegexManager,
    ) -> Vec<&NetworkFilter> {
        #[cfg(feature = "metrics")]
        let mut filters_checked = 0;
        #[cfg(feature = "metrics")]
        let mut filter_buckets = 0;

        let mut filters: Vec<&NetworkFilter> = vec![];

        #[cfg(not(feature = "metrics"))]
        {
            if self.filter_map.is_empty() {
                return filters;
            }
        }

        if let Some(source_hostname_hashes) = request.source_hostname_hashes.as_ref() {
            for token in source_hostname_hashes {
                if let Some(filter_bucket) = self.filter_map.get(token) {
                    #[cfg(feature = "metrics")]
                    {
                        filter_buckets += 1;
                    }

                    for filter in filter_bucket {
                        #[cfg(feature = "metrics")]
                        {
                            filters_checked += 1;
                        }
                        // if matched, also needs to be tagged with an active tag (or not tagged at all)
                        if filter.matches(request, regex_manager) && filter.tag.as_ref().map(|t| active_tags.contains(t)).unwrap_or(true) {
                            #[cfg(feature = "metrics")]
                            print!("true\t{}\t{}\tskipped\t{}\t{}\t", filter_buckets, filters_checked, filter_buckets, filters_checked);
                            filters.push(filter);
                        }
                    }
                }
            }
        }

        #[cfg(feature = "metrics")]
        print!("false\t{}\t{}\t", filter_buckets, filters_checked);

        for token in request_tokens {
            if let Some(filter_bucket) = self.filter_map.get(token) {
                #[cfg(feature = "metrics")]
                {
                    filter_buckets += 1;
                }
                for filter in filter_bucket {
                    #[cfg(feature = "metrics")]
                    {
                        filters_checked += 1;
                    }
                    // if matched, also needs to be tagged with an active tag (or not tagged at all)
                    if filter.matches(request, regex_manager) && filter.tag.as_ref().map(|t| active_tags.contains(t)).unwrap_or(true) {
                        #[cfg(feature = "metrics")]
                        print!("true\t{}\t{}\t", filter_buckets, filters_checked);
                        filters.push(filter);
                    }
                }
            }
        }

        #[cfg(feature = "metrics")]
        print!("false\t{}\t{}\t", filter_buckets, filters_checked);

        filters
    }
}

/// Inserts a value into the `Vec` under the specified key in the `HashMap`. The entry will be
/// created if it does not exist. If it already exists, it will be inserted in the `Vec` in a
/// sorted order.
fn insert_dup<K, V, H: std::hash::BuildHasher>(map: &mut HashMap<K, Vec<V>, H>, k: K, v: V)
where
    K: std::cmp::Ord + std::hash::Hash,
    V: PartialOrd,
{
    let entry = map.entry(k).or_insert_with(Vec::new);

    match entry.binary_search_by(|f| f.partial_cmp(&v).unwrap_or(std::cmp::Ordering::Equal)) {
        Ok(_pos) => (), // Can occur if the exact same rule is inserted twice. No reason to add anything.
        Err(slot) => entry.insert(slot, v),
    }
}

fn vec_hashmap_len<K: std::cmp::Eq + std::hash::Hash, V, H: std::hash::BuildHasher>(map: &HashMap<K, Vec<V>, H>) -> usize {
    let mut size = 0usize;
    for (_, val) in map.iter() {
        size += val.len();
    }
    size
}

fn token_histogram<T>(filter_tokens: &[(T, Vec<Vec<Hash>>)]) -> (u32, HashMap<Hash, u32>) {
    let mut tokens_histogram: HashMap<Hash, u32> = HashMap::new();
    let mut number_of_tokens = 0;
    for (_, tokens) in filter_tokens.iter() {
        for tg in tokens {
            for t in tg {
                *tokens_histogram.entry(*t).or_insert(0) += 1;
                number_of_tokens += 1;
            }
        }
    }

    for bad_token in ["http", "https", "www", "com"].iter() {
        tokens_histogram.insert(fast_hash(bad_token), number_of_tokens);
    }

    (number_of_tokens, tokens_histogram)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_dup_works() {
        let mut dup_map: HashMap<Hash, Vec<String>> = HashMap::new();

        // inserts into empty
        insert_dup(&mut dup_map, 1, String::from("foo"));
        assert_eq!(dup_map.get(&1), Some(&vec![String::from("foo")]));

        // adds item
        insert_dup(&mut dup_map, 1, String::from("bar"));
        assert_eq!(
            dup_map.get(&1),
            Some(&vec![String::from("bar"), String::from("foo")])
        );

        // inserts into another key item
        insert_dup(&mut dup_map, 123, String::from("baz"));
        assert_eq!(dup_map.get(&123), Some(&vec![String::from("baz")]));
        assert_eq!(
            dup_map.get(&1),
            Some(&vec![String::from("bar"), String::from("foo")])
        );
    }

    #[test]
    fn token_histogram_works() {
        // handle the case of just 1 token
        {
            let tokens = vec![(0, vec![vec![111]])];
            let (total_tokens, histogram) = token_histogram(&tokens);
            assert_eq!(total_tokens, 1);
            assert_eq!(histogram.get(&111), Some(&1));
            // include bad tokens
            assert_eq!(histogram.get(&fast_hash("http")), Some(&1));
            assert_eq!(histogram.get(&fast_hash("www")), Some(&1));
        }

        // handle the case of repeating tokens
        {
            let tokens = vec![(0, vec![vec![111]]), (1, vec![vec![111]])];
            let (total_tokens, histogram) = token_histogram(&tokens);
            assert_eq!(total_tokens, 2);
            assert_eq!(histogram.get(&111), Some(&2));
            // include bad tokens
            assert_eq!(histogram.get(&fast_hash("http")), Some(&2));
            assert_eq!(histogram.get(&fast_hash("www")), Some(&2));
        }

        // handle the different token set sizes
        {
            let tokens = vec![
                (0, vec![vec![111, 123, 132]]),
                (1, vec![vec![111], vec![123], vec![132]]),
                (2, vec![vec![111, 123], vec![132]]),
                (3, vec![vec![111, 111], vec![111]]),
            ];
            let (total_tokens, histogram) = token_histogram(&tokens);
            assert_eq!(total_tokens, 12);
            assert_eq!(histogram.get(&111), Some(&6));
            assert_eq!(histogram.get(&123), Some(&3));
            assert_eq!(histogram.get(&132), Some(&3));
            // include bad tokens
            assert_eq!(histogram.get(&fast_hash("http")), Some(&12));
            assert_eq!(histogram.get(&fast_hash("www")), Some(&12));
        }
    }

    #[test]
    fn network_filter_list_new_works() {
        {
            let filters = vec!["||foo.com"];
            let network_filters: Vec<_> = filters
                .into_iter()
                .map(|f| NetworkFilter::parse(&f, true, Default::default()))
                .filter_map(Result::ok)
                .collect();
            let filter_list = NetworkFilterList::new(network_filters, false);
            let maybe_matching_filter = filter_list.filter_map.get(&fast_hash("foo"));
            assert!(maybe_matching_filter.is_some(), "Expected filter not found");
        }
        // choses least frequent token
        {
            let filters = vec!["||foo.com", "||bar.com/foo"];
            let network_filters: Vec<_> = filters
                .into_iter()
                .map(|f| NetworkFilter::parse(&f, true, Default::default()))
                .filter_map(Result::ok)
                .collect();
            let filter_list = NetworkFilterList::new(network_filters, false);
            assert_eq!(
                filter_list.filter_map.get(&fast_hash("bar")).unwrap().len(),
                1
            );
            assert_eq!(
                filter_list.filter_map.get(&fast_hash("foo")).unwrap().len(),
                1
            );
        }
        // choses blacklisted token when no other choice
        {
            let filters = vec!["||foo.com", "||foo.com/bar", "||www"];
            let network_filters: Vec<_> = filters
                .into_iter()
                .map(|f| NetworkFilter::parse(&f, true, Default::default()))
                .filter_map(Result::ok)
                .collect();
            let filter_list = NetworkFilterList::new(network_filters, false);
            assert!(
                filter_list.filter_map.get(&fast_hash("www")).is_some(),
                "Filter matching {} not found",
                "www"
            );
            assert_eq!(
                filter_list.filter_map.get(&fast_hash("www")).unwrap().len(),
                1
            );
        }
        // uses domain as token when only one domain
        {
            let filters = vec!["||foo.com", "||foo.com$domain=bar.com"];
            let network_filters: Vec<_> = filters
                .into_iter()
                .map(|f| NetworkFilter::parse(&f, true, Default::default()))
                .filter_map(Result::ok)
                .collect();
            let filter_list = NetworkFilterList::new(network_filters, false);
            assert!(
                filter_list.filter_map.get(&fast_hash("bar.com")).is_some(),
                "Filter matching {} not found",
                "bar.com"
            );
            assert_eq!(
                filter_list
                    .filter_map
                    .get(&fast_hash("bar.com"))
                    .unwrap()
                    .len(),
                1
            );
        }
        // dispatches filter to multiple buckets per domain options if no token in main part
        {
            let filters = vec!["foo*$domain=bar.com|baz.com"];
            let network_filters: Vec<_> = filters
                .into_iter()
                .map(|f| NetworkFilter::parse(&f, true, Default::default()))
                .filter_map(Result::ok)
                .collect();
            let filter_list = NetworkFilterList::new(network_filters, false);
            assert_eq!(filter_list.filter_map.len(), 2);
            assert!(
                filter_list.filter_map.get(&fast_hash("bar.com")).is_some(),
                "Filter matching {} not found",
                "bar.com"
            );
            assert_eq!(
                filter_list
                    .filter_map
                    .get(&fast_hash("bar.com"))
                    .unwrap()
                    .len(),
                1
            );
            assert!(
                filter_list.filter_map.get(&fast_hash("baz.com")).is_some(),
                "Filter matching {} not found",
                "baz.com"
            );
            assert_eq!(
                filter_list
                    .filter_map
                    .get(&fast_hash("baz.com"))
                    .unwrap()
                    .len(),
                1
            );
        }
    }

    fn test_requests_filters(filters: &Vec<&str>, requests: &Vec<(Request, bool)>) {
        let network_filters: Vec<_> = filters
            .into_iter()
            .map(|f| NetworkFilter::parse(&f, true, Default::default()))
            .filter_map(Result::ok)
            .collect();
        let filter_list = NetworkFilterList::new(network_filters, false);
        let mut regex_manager = RegexManager::default();

        requests.into_iter().for_each(|(req, expected_result)| {
            let mut tokens = Vec::new();
            req.get_tokens(&mut tokens);
            let matched_rule =
                filter_list.check(&req, &tokens, &HashSet::new(), &mut regex_manager);
            if *expected_result {
                assert!(matched_rule.is_some(), "Expected match for {}", req.url);
            } else {
                assert!(matched_rule.is_none(), "Expected no match for {}, matched with {}", req.url, matched_rule.unwrap().to_string());
            }
        });
    }

    #[test]
    fn network_filter_list_check_works_plain_filter() {
        // includes cases with fall back to 0 bucket (no tokens from a rule)
        let filters = vec![
            "foo",
            "-foo-",
            "&fo.o=+_-",
            "foo/bar/baz",
            "com/bar/baz",
            "https://bar.com/bar/baz",
        ];

        let url_results = vec![
            ("https://bar.com/foo", true),
            ("https://bar.com/baz/foo", true),
            ("https://bar.com/q=foo/baz", true),
            ("https://foo.com", true),
            ("https://bar.com/baz/42-foo-q", true),
            ("https://bar.com?baz=42&fo.o=+_-", true),
            ("https://bar.com/foo/bar/baz", true),
            ("https://bar.com/bar/baz", true),
        ];

        let request_expectations: Vec<_> = url_results
            .into_iter()
            .map(|(url, expected_result)| {
                let request = Request::from_url(url).unwrap();
                (request, expected_result)
            })
            .collect();

        test_requests_filters(&filters, &request_expectations);
    }

    #[test]
    fn network_filter_list_check_works_hostname_anchor() {
        let filters = vec![
            "||foo.com",
            "||bar.com/bar",
            "||coo.baz.",
            "||foo.bar.com^",
            "||foo.baz^",
        ];

        let url_results = vec![
            ("https://foo.com/bar", true),
            ("https://bar.com/bar", true),
            ("https://baz.com/bar", false),
            ("https://baz.foo.com/bar", true),
            ("https://coo.baz.com/bar", true),
            ("https://foo.bar.com/bar", true),
            ("https://foo.baz.com/bar", false),
            ("https://baz.com", false),
            ("https://foo-bar.baz.com/bar", false),
            ("https://foo.de", false),
            ("https://bar.foo.de", false),
        ];

        let request_expectations: Vec<_> = url_results
            .into_iter()
            .map(|(url, expected_result)| {
                let request = Request::from_url(url).unwrap();
                (request, expected_result)
            })
            .collect();

        test_requests_filters(&filters, &request_expectations);
    }

    #[test]
    fn network_filter_list_check_works_unicode() {
        let filters = vec![
            "||firstrowsports.li/frame/",
            "||fırstrowsports.eu/pu/",
            "||atđhe.net/pu/",
        ];

        let url_results = vec![
            (
                Request::from_url("https://firstrowsports.li/frame/bar").unwrap(),
                true,
            ),
            (
                Request::from_url("https://secondrowsports.li/frame/bar").unwrap(),
                false,
            ),
            (
                Request::from_url("https://fırstrowsports.eu/pu/foo").unwrap(),
                true,
            ),
            (
                Request::from_url("https://xn--frstrowsports-39b.eu/pu/foo").unwrap(),
                true,
            ),
            (
                Request::from_url("https://atđhe.net/pu/foo").unwrap(),
                true,
            ),
            (
                Request::from_url("https://xn--athe-1ua.net/pu/foo").unwrap(),
                true,
            ),
        ];

        let request_expectations: Vec<_> = url_results
            .into_iter()
            .map(|(request, expected_result)| (request, expected_result))
            .collect();

        test_requests_filters(&filters, &request_expectations);
    }

    #[test]
    fn network_filter_list_check_works_regex_escaping() {
        let filters = vec![
            r#"/^https?:\/\/.*(bitly|bit)\.(com|ly)\/.*/$domain=123movies.com|1337x.to"#,
            r#"/\:\/\/data.*\.com\/[a-zA-Z0-9]{30,}/$third-party,xmlhttprequest"#
        ];

        let url_results = vec![
            (
                Request::from_urls("https://bit.ly/bar/", "http://123movies.com", "").unwrap(),
                true,
            ),
            (
                Request::from_urls(
                    "https://data.foo.com/9VjjrjU9Or2aqkb8PDiqTBnULPgeI48WmYEHkYer",
                    "http://123movies.com",
                    "xmlhttprequest",
                )
                .unwrap(),
                true,
            ),
        ];

        let request_expectations: Vec<_> = url_results
            .into_iter()
            .map(|(request, expected_result)| (request, expected_result))
            .collect();

        test_requests_filters(&filters, &request_expectations);
    }
}

#[cfg(test)]
mod blocker_tests {

    use super::*;
    use crate::lists::parse_filters;
    use crate::request::Request;
    use std::collections::HashSet;
    use std::iter::FromIterator;

    #[test]
    fn single_slash() {
        let filters = vec![
            String::from("/|"),
        ];

        let (network_filters, _) = parse_filters(&filters, true, Default::default());

        let blocker_options = BlockerOptions {
            enable_optimizations: true,
        };

        let blocker = Blocker::new(network_filters, &blocker_options);

        let request = Request::from_urls("https://example.com/test/", "https://example.com", "xmlhttprequest").unwrap();
        assert!(blocker.check(&request).matched);

        let request = Request::from_urls("https://example.com/test", "https://example.com", "xmlhttprequest").unwrap();
        assert!(!blocker.check(&request).matched);
    }

    fn test_requests_filters(filters: &[String], requests: &[(Request, bool)]) {
        let (network_filters, _) = parse_filters(filters, true, Default::default());

        let blocker_options: BlockerOptions = BlockerOptions {
            enable_optimizations: false,    // optimizations will reduce number of rules
        };

        let blocker = Blocker::new(network_filters, &blocker_options);

        requests.iter().for_each(|(req, expected_result)| {
            let matched_rule = blocker.check(&req);
            if *expected_result {
                assert!(matched_rule.matched, "Expected match for {}", req.url);
            } else {
                assert!(!matched_rule.matched, "Expected no match for {}, matched with {:?}", req.url, matched_rule.filter);
            }
        });
    }

    #[test]
    fn redirect_blocking_exception() {
        let filters = vec![
            String::from("||imdb-video.media-imdb.com$media,redirect=noop-0.1s.mp3"),
            String::from("@@||imdb-video.media-imdb.com^$domain=imdb.com"),
        ];

        let request = Request::from_urls("https://imdb-video.media-imdb.com/kBOeI88k1o23eNAi", "https://www.imdb.com/video/13", "media").unwrap();

        let (network_filters, _) = parse_filters(&filters, true, Default::default());

        let blocker_options: BlockerOptions = BlockerOptions {
            enable_optimizations: false,
        };

        let mut blocker = Blocker::new(network_filters, &blocker_options);

        blocker.add_resource(&Resource {
            name: "noop-0.1s.mp3".to_string(),
            aliases: vec![],
            kind: crate::resources::ResourceType::Mime(crate::resources::MimeType::AudioMp3),
            content: base64::encode("mp3"),
        }).unwrap();

        let matched_rule = blocker.check(&request);
        assert_eq!(matched_rule.matched, false);
        assert_eq!(matched_rule.important, false);
        assert_eq!(matched_rule.redirect, Some("data:audio/mp3;base64,bXAz".to_string()));
        assert_eq!(matched_rule.exception, Some("@@||imdb-video.media-imdb.com^$domain=imdb.com".to_string()));
        assert_eq!(matched_rule.error, None);
    }

    #[test]
    fn redirect_exception() {
        let filters = vec![
            String::from("||imdb-video.media-imdb.com$media,redirect=noop-0.1s.mp3"),
            String::from("@@||imdb-video.media-imdb.com^$domain=imdb.com,redirect=noop-0.1s.mp3"),
        ];

        let request = Request::from_urls("https://imdb-video.media-imdb.com/kBOeI88k1o23eNAi", "https://www.imdb.com/video/13", "media").unwrap();

        let (network_filters, _) = parse_filters(&filters, true, Default::default());

        let blocker_options: BlockerOptions = BlockerOptions {
            enable_optimizations: false,
        };

        let mut blocker = Blocker::new(network_filters, &blocker_options);

        blocker.add_resource(&Resource {
            name: "noop-0.1s.mp3".to_string(),
            aliases: vec![],
            kind: crate::resources::ResourceType::Mime(crate::resources::MimeType::AudioMp3),
            content: base64::encode("mp3"),
        }).unwrap();

        let matched_rule = blocker.check(&request);
        assert_eq!(matched_rule.matched, false);
        assert_eq!(matched_rule.important, false);
        assert_eq!(matched_rule.redirect, None);
        assert_eq!(matched_rule.exception, Some("@@||imdb-video.media-imdb.com^$domain=imdb.com,redirect=noop-0.1s.mp3".to_string()));
        assert_eq!(matched_rule.error, None);
    }

    #[test]
    fn redirect_rule_redirection() {
        let filters = vec![
            String::from("||doubleclick.net^"),
            String::from("||www3.doubleclick.net^$xmlhttprequest,redirect-rule=noop.txt,domain=lineups.fun"),
        ];

        let request = Request::from_urls("https://www3.doubleclick.net", "https://lineups.fun", "xhr").unwrap();

        let (network_filters, _) = parse_filters(&filters, true, Default::default());

        let blocker_options: BlockerOptions = BlockerOptions {
            enable_optimizations: false,
        };

        let mut blocker = Blocker::new(network_filters, &blocker_options);

        blocker.add_resource(&Resource {
            name: "noop.txt".to_string(),
            aliases: vec![],
            kind: crate::resources::ResourceType::Mime(crate::resources::MimeType::TextPlain),
            content: base64::encode("noop"),
        }).unwrap();

        let matched_rule = blocker.check(&request);
        assert_eq!(matched_rule.matched, true);
        assert_eq!(matched_rule.important, false);
        assert_eq!(matched_rule.redirect, Some("data:text/plain;base64,bm9vcA==".to_string()));
        assert_eq!(matched_rule.exception, None);
        assert_eq!(matched_rule.error, None);
    }

    #[test]
    fn badfilter_does_not_match() {
        let filters = vec![
            String::from("||foo.com$badfilter")
        ];
        let url_results = vec![
            (
                Request::from_urls("https://foo.com", "https://bar.com", "image").unwrap(),
                false,
            ),
        ];

        let request_expectations: Vec<_> = url_results
            .into_iter()
            .map(|(request, expected_result)| (request, expected_result))
            .collect();

        test_requests_filters(&filters, &request_expectations);
    }

    #[test]
    fn badfilter_cancels_with_same_id() {
        let filters = vec![
            String::from("||foo.com$domain=bar.com|foo.com,badfilter"),
            String::from("||foo.com$domain=foo.com|bar.com")
        ];
        let url_results = vec![
            (
                Request::from_urls("https://foo.com", "https://bar.com", "image").unwrap(),
                false,
            ),
        ];

        let request_expectations: Vec<_> = url_results
            .into_iter()
            .map(|(request, expected_result)| (request, expected_result))
            .collect();

        test_requests_filters(&filters, &request_expectations);
    }

    #[test]
    fn badfilter_does_not_cancel_similar_filter() {
        let filters = vec![
            String::from("||foo.com$domain=bar.com|foo.com,badfilter"),
            String::from("||foo.com$domain=foo.com|bar.com,image")
        ];
        let url_results = vec![
            (
                Request::from_urls("https://foo.com", "https://bar.com", "image").unwrap(),
                true,
            ),
        ];

        let request_expectations: Vec<_> = url_results
            .into_iter()
            .map(|(request, expected_result)| (request, expected_result))
            .collect();

        test_requests_filters(&filters, &request_expectations);
    }

    #[test]
    fn hostname_regex_filter_works() {
        let filters = vec![
            String::from("||alimc*.top^$domain=letv.com"),
            String::from("||aa*.top^$domain=letv.com")
        ];
        let url_results = vec![
            (Request::from_urls("https://r.alimc1.top/test.js", "https://minisite.letv.com/", "script").unwrap(), true),
            (Request::from_urls("https://www.baidu.com/test.js", "https://minisite.letv.com/", "script").unwrap(), false),
            (Request::from_urls("https://r.aabb.top/test.js", "https://example.com/", "script").unwrap(), false),
            (Request::from_urls("https://r.aabb.top/test.js", "https://minisite.letv.com/", "script").unwrap(), true),
        ];

        let (network_filters, _) = parse_filters(&filters, true, Default::default());

        let blocker_options = BlockerOptions {
            enable_optimizations: false,    // optimizations will reduce number of rules
        };

        let blocker = Blocker::new(network_filters, &blocker_options);

        url_results.into_iter().for_each(|(req, expected_result)| {
            let matched_rule = blocker.check(&req);
            if expected_result {
                assert!(matched_rule.matched, "Expected match for {}", req.url);
            } else {
                assert!(!matched_rule.matched, "Expected no match for {}, matched with {:?}", req.url, matched_rule.filter);
            }
        });
    }

    #[test]
    fn get_csp_directives() {
        let filters = vec![
            String::from("$csp=script-src 'self' * 'unsafe-inline',domain=thepiratebay.vip|pirateproxy.live|thehiddenbay.com|downloadpirate.com|thepiratebay10.org|kickass.vip|pirateproxy.app|ukpass.co|prox.icu|pirateproxy.life"),
            String::from("$csp=worker-src 'none',domain=pirateproxy.live|thehiddenbay.com|tpb.party|thepiratebay.org|thepiratebay.vip|thepiratebay10.org|flashx.cc|vidoza.co|vidoza.net"),
            String::from("||1337x.to^$csp=script-src 'self' 'unsafe-inline'"),
            String::from("@@^no-csp^$csp=script-src 'self' 'unsafe-inline'"),
            String::from("^duplicated-directive^$csp=worker-src 'none'"),
            String::from("@@^disable-all^$csp"),
            String::from("^first-party-only^$csp=script-src 'none',1p"),
        ];

        let (network_filters, _) = parse_filters(&filters, true, Default::default());

        let blocker_options = BlockerOptions {
            enable_optimizations: false,
        };

        let blocker = Blocker::new(network_filters, &blocker_options);

        {   // No directives should be returned for requests that are not `document` or `subdocument` content types.
            assert_eq!(blocker.get_csp_directives(&Request::from_urls("https://pirateproxy.live/static/custom_ads.js", "https://pirateproxy.live", "script").unwrap()), None);
            assert_eq!(blocker.get_csp_directives(&Request::from_urls("https://pirateproxy.live/static/custom_ads.js", "https://pirateproxy.live", "image").unwrap()), None);
            assert_eq!(blocker.get_csp_directives(&Request::from_urls("https://pirateproxy.live/static/custom_ads.js", "https://pirateproxy.live", "object").unwrap()), None);
        }
        {   // A single directive should be returned if only one match is present in the engine, for both document and subdocument types
            assert_eq!(blocker.get_csp_directives(&Request::from_urls("https://example.com", "https://vidoza.co", "document").unwrap()), Some(String::from("worker-src 'none'")));
            assert_eq!(blocker.get_csp_directives(&Request::from_urls("https://example.com", "https://vidoza.net", "subdocument").unwrap()), Some(String::from("worker-src 'none'")));
        }
        {   // Multiple merged directives should be returned if more than one match is present in the engine
            let possible_results = vec![
                Some(String::from("script-src 'self' * 'unsafe-inline',worker-src 'none'")),
                Some(String::from("worker-src 'none',script-src 'self' * 'unsafe-inline'")),
            ];
            assert!(possible_results.contains(&blocker.get_csp_directives(&Request::from_urls("https://example.com", "https://pirateproxy.live", "document").unwrap())));
            assert!(possible_results.contains(&blocker.get_csp_directives(&Request::from_urls("https://example.com", "https://pirateproxy.live", "subdocument").unwrap())));
        }
        {   // A directive with an exception should not be returned
            assert_eq!(blocker.get_csp_directives(&Request::from_urls("https://1337x.to", "https://1337x.to", "document").unwrap()), Some(String::from("script-src 'self' 'unsafe-inline'")));
            assert_eq!(blocker.get_csp_directives(&Request::from_urls("https://1337x.to/no-csp", "https://1337x.to", "subdocument").unwrap()), None);
        }
        {   // Multiple identical directives should only appear in the output once
            assert_eq!(blocker.get_csp_directives(&Request::from_urls("https://example.com/duplicated-directive", "https://flashx.cc", "document").unwrap()), Some(String::from("worker-src 'none'")));
            assert_eq!(blocker.get_csp_directives(&Request::from_urls("https://example.com/duplicated-directive", "https://flashx.cc", "subdocument").unwrap()), Some(String::from("worker-src 'none'")));
        }
        {   // A CSP exception with no corresponding directive should disable all CSP injections for the page
            assert_eq!(blocker.get_csp_directives(&Request::from_urls("https://1337x.to/duplicated-directive/disable-all", "https://thepiratebay10.org", "document").unwrap()), None);
            assert_eq!(blocker.get_csp_directives(&Request::from_urls("https://1337x.to/duplicated-directive/disable-all", "https://thepiratebay10.org", "document").unwrap()), None);
        }
        {   // A CSP exception with a partyness modifier should only match where the modifier applies
            assert_eq!(blocker.get_csp_directives(&Request::from_urls("htps://github.com/first-party-only", "https://example.com", "subdocument").unwrap()), None);
            assert_eq!(blocker.get_csp_directives(&Request::from_urls("https://example.com/first-party-only", "https://example.com", "document").unwrap()), Some(String::from("script-src 'none'")));
        }
    }

    #[test]
    fn test_removeparam() {
        let filters = vec![
            String::from("||example.com^$removeparam=test"),
            String::from("*$removeparam=fbclid"),
            String::from("/script.js$redirect-rule=noopjs"),
            String::from("^block^$important"),
            String::from("$removeparam=testCase,~image"),
        ];

        let (network_filters, _) = parse_filters(&filters, true, Default::default());

        let blocker_options = BlockerOptions {
            enable_optimizations: true,
        };

        let mut blocker = Blocker::new(network_filters, &blocker_options);
        blocker.add_resource(&Resource {
            name: "noopjs".into(),
            aliases: vec![],
            kind: crate::resources::ResourceType::Mime(crate::resources::MimeType::ApplicationJavascript),
            content: base64::encode("(() => {})()"),
        }).unwrap();

        let result = blocker.check(&Request::from_urls("https://example.com?q=1&test=2#blue", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, Some("https://example.com?q=1#blue".into()));
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com?test=2&q=1#blue", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, Some("https://example.com?q=1#blue".into()));
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com?test=2#blue", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, Some("https://example.com#blue".into()));
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com?q=1#blue", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, None);
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com?q=1&test=2", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, Some("https://example.com?q=1".into()));
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com?test=2&q=1", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, Some("https://example.com?q=1".into()));
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com?test=2", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, Some("https://example.com".into()));
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com?q=1", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, None);
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com?q=fbclid", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, None);
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com?fbclid=10938&q=1&test=2", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, Some("https://example.com?q=1".into()));
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://test.com?fbclid=10938&q=1&test=2", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, Some("https://test.com?q=1&test=2".into()));
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com?q1=1&q2=2&q3=3&test=2&q4=4&q5=5&fbclid=39", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, Some("https://example.com?q1=1&q2=2&q3=3&q4=4&q5=5".into()));
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com?q1=1&q1=2&test=2&test=3", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, Some("https://example.com?q1=1&q1=2".into()));
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com/script.js?test=2#blue", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, Some("https://example.com/script.js#blue".into()));
        assert_eq!(result.redirect, Some("data:application/javascript;base64,KCgpID0+IHt9KSgp".into()));
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com/block/script.js?test=2", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, None);
        assert_eq!(result.redirect, Some("data:application/javascript;base64,KCgpID0+IHt9KSgp".into()));
        assert!(result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com/Path/?Test=ABC&testcase=AbC&testCase=aBc", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, Some("https://example.com/Path/?Test=ABC&testcase=AbC".into()));
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com?Test=ABC?123&test=3#&test=4#b", "https://antonok.com", "script").unwrap());
        assert_eq!(result.rewritten_url, Some("https://example.com?Test=ABC?123#&test=4#b".into()));
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com?Test=ABC&testCase=5", "https://antonok.com", "document").unwrap());
        assert_eq!(result.rewritten_url, Some("https://example.com?Test=ABC".into()));
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com?Test=ABC&testCase=5", "https://antonok.com", "image").unwrap());
        assert_eq!(result.rewritten_url, None);
        assert!(!result.matched);
    }

    /// Tests ported from the previous query parameter stripping logic in brave-core
    #[test]
    fn removeparam_brave_core_tests() {
        let testcases = vec![
            // (original url, expected url after filtering)
            ("https://example.com/?fbclid=1234", "https://example.com/"),
            ("https://example.com/?fbclid=1234&", "https://example.com/"),
            ("https://example.com/?&fbclid=1234", "https://example.com/"),
            ("https://example.com/?gclid=1234", "https://example.com/"),
            ("https://example.com/?fbclid=0&gclid=1&msclkid=a&mc_eid=a1",
             "https://example.com/"),
            ("https://example.com/?fbclid=&foo=1&bar=2&gclid=abc",
             "https://example.com/?fbclid=&foo=1&bar=2"),
            ("https://example.com/?fbclid=&foo=1&gclid=1234&bar=2",
             "https://example.com/?fbclid=&foo=1&bar=2"),
            ("http://u:p@example.com/path/file.html?foo=1&fbclid=abcd#fragment",
             "http://u:p@example.com/path/file.html?foo=1#fragment"),
            ("https://example.com/?__s=1234-abcd", "https://example.com/"),
            // Obscure edge cases that break most parsers:
            ("https://example.com/?fbclid&foo&&gclid=2&bar=&%20",
             "https://example.com/?fbclid&foo&&bar=&%20"),
            ("https://example.com/?fbclid=1&1==2&=msclkid&foo=bar&&a=b=c&",
             "https://example.com/?1==2&=msclkid&foo=bar&&a=b=c&"),
            ("https://example.com/?fbclid=1&=2&?foo=yes&bar=2+",
             "https://example.com/?=2&?foo=yes&bar=2+"),
            ("https://example.com/?fbclid=1&a+b+c=some%20thing&1%202=3+4",
             "https://example.com/?a+b+c=some%20thing&1%202=3+4"),
            // Conditional query parameter stripping
            /*("https://example.com/?mkt_tok=123&foo=bar",
             "https://example.com/?foo=bar"),*/
        ];

        let filters = [
            "fbclid", "gclid", "msclkid", "mc_eid",
            "dclid",
            "oly_anon_id", "oly_enc_id",
            "_openstat",
            "vero_conv", "vero_id",
            "wickedid",
            "yclid",
            "__s",
            "rb_clickid",
            "s_cid",
            "ml_subscriber", "ml_subscriber_hash",
            "twclid",
            "gbraid", "wbraid",
            "_hsenc", "__hssc", "__hstc", "__hsfp", "hsCtaTracking",
            "oft_id", "oft_k", "oft_lk", "oft_d", "oft_c", "oft_ck", "oft_ids",
            "oft_sk",
            "ss_email_id",
            "bsft_uid", "bsft_clkid",
            "vgo_ee",
            "igshid",
        ].iter().map(|s| format!("*$removeparam={}", s)).collect::<Vec<_>>();

        let (network_filters, _) = parse_filters(&filters, true, Default::default());

        let blocker_options = BlockerOptions {
            enable_optimizations: true,
        };

        let blocker = Blocker::new(network_filters, &blocker_options);

        for (original, expected) in testcases.into_iter() {
            let result = blocker.check(&Request::from_urls(original, "https://example.net", "script").unwrap());
            let expected = if original == expected {
                None
            } else {
                Some(expected.to_string())
            };
            assert_eq!(expected, result.rewritten_url, "Filtering parameters on {} failed", original);
        }
    }

    #[test]
    fn test_redirect_priority() {
        let filters = vec![
            String::from(".txt^$redirect-rule=a"),
            String::from("||example.com^$redirect-rule=b:10"),
            String::from("/text$redirect-rule=c:20"),
            String::from("@@^excepta^$redirect-rule=a"),
            String::from("@@^exceptb10^$redirect-rule=b:10"),
            String::from("@@^exceptc20^$redirect-rule=c:20"),
        ];

        let (network_filters, _) = parse_filters(&filters, true, Default::default());

        let blocker_options = BlockerOptions {
            enable_optimizations: true,
        };

        let mut blocker = Blocker::new(network_filters, &blocker_options);
        fn add_simple_resource(blocker: &mut Blocker, identifier: &str) -> Option<String> {
            let b64 = base64::encode(identifier);
            blocker.add_resource(&Resource {
                name: identifier.into(),
                aliases: vec![],
                kind: crate::resources::ResourceType::Mime(crate::resources::MimeType::TextPlain),
                content: base64::encode(identifier),
            }).unwrap();
            return Some(format!("data:text/plain;base64,{}", b64));
        }
        let a_redirect = add_simple_resource(&mut blocker, "a");
        let b_redirect = add_simple_resource(&mut blocker, "b");
        let c_redirect = add_simple_resource(&mut blocker, "c");

        let result = blocker.check(&Request::from_urls("https://example.net/test", "https://example.com", "xmlhttprequest").unwrap());
        assert_eq!(result.redirect, None);
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.net/test.txt", "https://example.com", "xmlhttprequest").unwrap());
        assert_eq!(result.redirect, a_redirect);
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com/test.txt", "https://example.com", "xmlhttprequest").unwrap());
        assert_eq!(result.redirect, b_redirect);
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com/text.txt", "https://example.com", "xmlhttprequest").unwrap());
        assert_eq!(result.redirect, c_redirect);
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com/exceptc20/text.txt", "https://example.com", "xmlhttprequest").unwrap());
        assert_eq!(result.redirect, b_redirect);
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com/exceptb10/text.txt", "https://example.com", "xmlhttprequest").unwrap());
        assert_eq!(result.redirect, c_redirect);
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com/exceptc20/exceptb10/text.txt", "https://example.com", "xmlhttprequest").unwrap());
        assert_eq!(result.redirect, a_redirect);
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com/exceptc20/exceptb10/excepta/text.txt", "https://example.com", "xmlhttprequest").unwrap());
        assert_eq!(result.redirect, None);
        assert!(!result.matched);

        let result = blocker.check(&Request::from_urls("https://example.com/exceptc20/exceptb10/text", "https://example.com", "xmlhttprequest").unwrap());
        assert_eq!(result.redirect, None);
        assert!(!result.matched);
    }

    #[test]
    fn tags_enable_works() {
        let filters = vec![
            String::from("adv$tag=stuff"),
            String::from("somelongpath/test$tag=stuff"),
            String::from("||brianbondy.com/$tag=brian"),
            String::from("||brave.com$tag=brian"),
        ];
        let url_results = vec![
            (Request::from_url("http://example.com/advert.html").unwrap(), true),
            (Request::from_url("http://example.com/somelongpath/test/2.html").unwrap(), true),
            (Request::from_url("https://brianbondy.com/about").unwrap(), false),
            (Request::from_url("https://brave.com/about").unwrap(), false),
        ];

        let (network_filters, _) = parse_filters(&filters, true, Default::default());

        let blocker_options: BlockerOptions = BlockerOptions {
            enable_optimizations: false,    // optimizations will reduce number of rules
        };

        let mut blocker = Blocker::new(network_filters, &blocker_options);
        blocker.enable_tags(&["stuff"]);
        assert_eq!(blocker.tags_enabled, HashSet::from_iter(vec![String::from("stuff")].into_iter()));
        assert_eq!(vec_hashmap_len(&blocker.filters_tagged.filter_map), 2);

        url_results.into_iter().for_each(|(req, expected_result)| {
            let matched_rule = blocker.check(&req);
            if expected_result {
                assert!(matched_rule.matched, "Expected match for {}", req.url);
            } else {
                assert!(!matched_rule.matched, "Expected no match for {}, matched with {:?}", req.url, matched_rule.filter);
            }
        });
    }

    #[test]
    fn tags_enable_adds_tags() {
        let filters = vec![
            String::from("adv$tag=stuff"),
            String::from("somelongpath/test$tag=stuff"),
            String::from("||brianbondy.com/$tag=brian"),
            String::from("||brave.com$tag=brian"),
        ];
        let url_results = vec![
            (Request::from_url("http://example.com/advert.html").unwrap(), true),
            (Request::from_url("http://example.com/somelongpath/test/2.html").unwrap(), true),
            (Request::from_url("https://brianbondy.com/about").unwrap(), true),
            (Request::from_url("https://brave.com/about").unwrap(), true),
        ];

        let (network_filters, _) = parse_filters(&filters, true, Default::default());

        let blocker_options: BlockerOptions = BlockerOptions {
            enable_optimizations: false,    // optimizations will reduce number of rules
        };

        let mut blocker = Blocker::new(network_filters, &blocker_options);
        blocker.enable_tags(&["stuff"]);
        blocker.enable_tags(&["brian"]);
        assert_eq!(blocker.tags_enabled, HashSet::from_iter(vec![String::from("brian"), String::from("stuff")].into_iter()));
        assert_eq!(vec_hashmap_len(&blocker.filters_tagged.filter_map), 4);

        url_results.into_iter().for_each(|(req, expected_result)| {
            let matched_rule = blocker.check(&req);
            if expected_result {
                assert!(matched_rule.matched, "Expected match for {}", req.url);
            } else {
                assert!(!matched_rule.matched, "Expected no match for {}, matched with {:?}", req.url, matched_rule.filter);
            }
        });
    }

    #[test]
    fn tags_disable_works() {
        let filters = vec![
            String::from("adv$tag=stuff"),
            String::from("somelongpath/test$tag=stuff"),
            String::from("||brianbondy.com/$tag=brian"),
            String::from("||brave.com$tag=brian"),
        ];
        let url_results = vec![
            (Request::from_url("http://example.com/advert.html").unwrap(), false),
            (Request::from_url("http://example.com/somelongpath/test/2.html").unwrap(), false),
            (Request::from_url("https://brianbondy.com/about").unwrap(), true),
            (Request::from_url("https://brave.com/about").unwrap(), true),
        ];

        let (network_filters, _) = parse_filters(&filters, true, Default::default());

        let blocker_options: BlockerOptions = BlockerOptions {
            enable_optimizations: false,    // optimizations will reduce number of rules
        };

        let mut blocker = Blocker::new(network_filters, &blocker_options);
        blocker.enable_tags(&["brian", "stuff"]);
        assert_eq!(blocker.tags_enabled, HashSet::from_iter(vec![String::from("brian"), String::from("stuff")].into_iter()));
        assert_eq!(vec_hashmap_len(&blocker.filters_tagged.filter_map), 4);
        blocker.disable_tags(&["stuff"]);
        assert_eq!(blocker.tags_enabled, HashSet::from_iter(vec![String::from("brian")].into_iter()));
        assert_eq!(vec_hashmap_len(&blocker.filters_tagged.filter_map), 2);

        url_results.into_iter().for_each(|(req, expected_result)| {
            let matched_rule = blocker.check(&req);
            if expected_result {
                assert!(matched_rule.matched, "Expected match for {}", req.url);
            } else {
                assert!(!matched_rule.matched, "Expected no match for {}, matched with {:?}", req.url, matched_rule.filter);
            }
        });
    }

    #[test]
    fn filter_add_badfilter_error() {
        let blocker_options: BlockerOptions = BlockerOptions {
            enable_optimizations: false,
        };

        let mut blocker = Blocker::new(Vec::new(), &blocker_options);

        let filter = NetworkFilter::parse("adv$badfilter", true, Default::default()).unwrap();
        let added = blocker.add_filter(filter);
        assert!(added.is_err());
        assert_eq!(added.err().unwrap(), BlockerError::BadFilterAddUnsupported);
    }

    #[test]
    #[ignore]
    fn filter_add_twice_handling_error() {
        {
            // Not allow filter to be added twice hwn the engine is not optimised
            let blocker_options: BlockerOptions = BlockerOptions {
                enable_optimizations: false,
            };

            let mut blocker = Blocker::new(Vec::new(), &blocker_options);

            let filter = NetworkFilter::parse("adv", true, Default::default()).unwrap();
            blocker.add_filter(filter.clone()).unwrap();
            assert!(blocker.filter_exists(&filter), "Expected filter to be inserted");
            let added = blocker.add_filter(filter);
            assert!(added.is_err(), "Expected repeated insertion to fail");
            assert_eq!(added.err().unwrap(), BlockerError::FilterExists, "Expected specific error on repeated insertion fail");
        }
        {
            // Allow filter to be added twice when the engine is optimised
            let blocker_options: BlockerOptions = BlockerOptions {
                enable_optimizations: true,
            };

            let mut blocker = Blocker::new(Vec::new(), &blocker_options);

            let filter = NetworkFilter::parse("adv", true, Default::default()).unwrap();
            blocker.add_filter(filter.clone()).unwrap();
            let added = blocker.add_filter(filter);
            assert!(added.is_ok());
        }
    }

    #[test]
    fn filter_add_tagged() {
        // Allow filter to be added twice when the engine is optimised
        let blocker_options: BlockerOptions = BlockerOptions {
            enable_optimizations: true,
        };

        let mut blocker = Blocker::new(Vec::new(), &blocker_options);
        blocker.enable_tags(&["brian"]);

        blocker.add_filter(NetworkFilter::parse("adv$tag=stuff", true, Default::default()).unwrap()).unwrap();
        blocker.add_filter(NetworkFilter::parse("somelongpath/test$tag=stuff", true, Default::default()).unwrap()).unwrap();
        blocker.add_filter(NetworkFilter::parse("||brianbondy.com/$tag=brian", true, Default::default()).unwrap()).unwrap();
        blocker.add_filter(NetworkFilter::parse("||brave.com$tag=brian", true, Default::default()).unwrap()).unwrap();

        let url_results = vec![
            (Request::from_url("http://example.com/advert.html").unwrap(), false),
            (Request::from_url("http://example.com/somelongpath/test/2.html").unwrap(), false),
            (Request::from_url("https://brianbondy.com/about").unwrap(), true),
            (Request::from_url("https://brave.com/about").unwrap(), true),
        ];

        url_results.into_iter().for_each(|(req, expected_result)| {
            let matched_rule = blocker.check(&req);
            if expected_result {
                assert!(matched_rule.matched, "Expected match for {}", req.url);
            } else {
                assert!(!matched_rule.matched, "Expected no match for {}, matched with {:?}", req.url, matched_rule.filter);
            }
        });
    }

    #[test]
    fn exception_force_check() {
        let blocker_options: BlockerOptions = BlockerOptions {
            enable_optimizations: true,
        };

        let mut blocker = Blocker::new(Vec::new(), &blocker_options);

        blocker.add_filter(NetworkFilter::parse("@@*ad_banner.png", true, Default::default()).unwrap()).unwrap();

        let request = Request::from_url("http://example.com/ad_banner.png").unwrap();

        let matched_rule = blocker.check_parameterised(&request, false, true);
        assert!(!matched_rule.matched);
        assert!(matched_rule.exception.is_some());
    }

    #[test]
    fn generichide() {
        let blocker_options: BlockerOptions = BlockerOptions {
            enable_optimizations: true,
        };

        let mut blocker = Blocker::new(Vec::new(), &blocker_options);

        blocker.add_filter(NetworkFilter::parse("@@||example.com$generichide", true, Default::default()).unwrap()).unwrap();

        assert!(blocker.check_generic_hide(&Request::from_url("https://example.com").unwrap()));
    }
}

#[cfg(test)]
mod legacy_rule_parsing_tests {
    use crate::utils::rules_from_lists;
    use crate::lists::{parse_filters, FilterFormat, ParseOptions};
    use crate::blocker::{Blocker, BlockerOptions};
    use crate::blocker::vec_hashmap_len;

    struct ListCounts {
        pub filters: usize,
        pub cosmetic_filters: usize,
        pub exceptions: usize,
        pub duplicates: usize,
    }

    impl std::ops::Add<ListCounts> for ListCounts {
        type Output = ListCounts;

        fn add(self, other: ListCounts) -> Self::Output {
            ListCounts {
                filters: self.filters + other.filters,
                cosmetic_filters: self.cosmetic_filters + other.cosmetic_filters,
                exceptions: self.exceptions + other.exceptions,
                duplicates: 0,  // Don't bother trying to calculate - lists could have cross-duplicated entries
            }
        }
    }

    // number of expected EasyList cosmetic rules from old engine is 31144, but is incorrect as it skips a few particularly long rules that are nevertheless valid
    // easyList = { 24478, 31144, 0, 5589 };
    // not handling (and not including) filters with the following options:
    // - $popup
    // - $elemhide
    // difference from original counts caused by not handling document/subdocument options and possibly miscounting on the blocker side.
    // Printing all non-cosmetic, non-html, non-comment/-empty rules and ones with no unsupported options yields 29142 items
    // This engine also handles 3 rules that old one does not
    const EASY_LIST: ListCounts = ListCounts { filters: 24064, cosmetic_filters: 31163, exceptions: 5796, duplicates: 0 };
    // easyPrivacy = { 11817, 0, 0, 1020 };
    // differences in counts explained by hashset size underreporting as detailed in the next two cases
    const EASY_PRIVACY: ListCounts = ListCounts { filters: 11889, cosmetic_filters: 0, exceptions: 1021, duplicates: 2 };
    // ublockUnbreak = { 4, 8, 0, 94 };
    // differences in counts explained by client.hostAnchoredExceptionHashSet->GetSize() underreporting when compared to client.numHostAnchoredExceptionFilters
    const UBLOCK_UNBREAK: ListCounts = ListCounts { filters: 4, cosmetic_filters: 8, exceptions: 98, duplicates: 0 };
    // braveUnbreak = { 31, 0, 0, 4 };
    // differences in counts explained by client.hostAnchoredHashSet->GetSize() underreporting when compared to client.numHostAnchoredFilters
    const BRAVE_UNBREAK: ListCounts = ListCounts { filters: 32, cosmetic_filters: 0, exceptions: 4, duplicates: 0 };
    // disconnectSimpleMalware = { 2450, 0, 0, 0 };
    const DISCONNECT_SIMPLE_MALWARE: ListCounts = ListCounts { filters: 2450, cosmetic_filters: 0, exceptions: 0, duplicates: 0 };
    // spam404MainBlacklist = { 5629, 166, 0, 0 };
    const SPAM_404_MAIN_BLACKLIST: ListCounts = ListCounts { filters: 5629, cosmetic_filters: 166, exceptions: 0, duplicates: 0 };
    const MALWARE_DOMAIN_LIST: ListCounts = ListCounts { filters: 1104, cosmetic_filters: 0, exceptions: 0, duplicates: 3 };
    const MALWARE_DOMAINS: ListCounts = ListCounts { filters: 26853, cosmetic_filters: 0, exceptions: 0, duplicates: 48 };

    fn check_list_counts(rule_lists: &[String], format: FilterFormat, expectation: ListCounts) {
        let rules = rules_from_lists(rule_lists);

        let (network_filters, cosmetic_filters) = parse_filters(&rules, true, ParseOptions { format, ..Default::default() });

        assert_eq!(
            (network_filters.len(),
            network_filters.iter().filter(|f| f.is_exception()).count(),
            cosmetic_filters.len()),
            (expectation.filters + expectation.exceptions,
            expectation.exceptions,
            expectation.cosmetic_filters),
            "Number of collected filters does not match expectation");

        let blocker_options = BlockerOptions {
            enable_optimizations: false,    // optimizations will reduce number of rules
        };

        let blocker = Blocker::new(network_filters, &blocker_options);

        // Some filters in the filter_map are pointed at by multiple tokens, increasing the total number of items
        assert!(vec_hashmap_len(&blocker.exceptions.filter_map) + vec_hashmap_len(&blocker.generic_hide.filter_map)
            >= expectation.exceptions, "Number of collected exceptions does not match expectation");

        assert!(vec_hashmap_len(&blocker.filters.filter_map) +
            vec_hashmap_len(&blocker.importants.filter_map) +
            vec_hashmap_len(&blocker.redirects.filter_map) +
            vec_hashmap_len(&blocker.redirects.filter_map) +
            vec_hashmap_len(&blocker.csp.filter_map) >=
            expectation.filters - expectation.duplicates, "Number of collected network filters does not match expectation");
    }

    #[test]
    fn parse_easylist() {
        check_list_counts(&vec![String::from("./data/test/easylist.txt")], FilterFormat::Standard, EASY_LIST);
    }

    #[test]
    fn parse_easyprivacy() {
        check_list_counts(&vec![String::from("./data/test/easyprivacy.txt")], FilterFormat::Standard, EASY_PRIVACY);
    }

    #[test]
    fn parse_ublock_unbreak() {
        check_list_counts(&vec![String::from("./data/test/ublock-unbreak.txt")], FilterFormat::Standard, UBLOCK_UNBREAK);
    }

    #[test]
    fn parse_brave_unbreak() {
        check_list_counts(&vec![String::from("./data/test/brave-unbreak.txt")], FilterFormat::Standard, BRAVE_UNBREAK);
    }

    #[test]
    fn parse_brave_disconnect_simple_malware() {
        check_list_counts(&vec![String::from("./data/test/disconnect-simple-malware.txt")], FilterFormat::Standard, DISCONNECT_SIMPLE_MALWARE);
    }

    #[test]
    fn parse_spam404_main_blacklist() {
        check_list_counts(&vec![String::from("./data/test/spam404-main-blacklist.txt")], FilterFormat::Standard, SPAM_404_MAIN_BLACKLIST);
    }

    #[test]
    fn parse_malware_domain_list() {
        check_list_counts(&vec![String::from("./data/test/malwaredomainlist.txt")], FilterFormat::Hosts, MALWARE_DOMAIN_LIST);
    }

    #[test]
    fn parse_malware_domain_list_just_hosts() {
        check_list_counts(&vec![String::from("./data/test/malwaredomainlist_justhosts.txt")], FilterFormat::Hosts, MALWARE_DOMAIN_LIST);
    }

    #[test]
    fn parse_malware_domains() {
        check_list_counts(&vec![String::from("./data/test/malwaredomains.txt")], FilterFormat::Hosts, MALWARE_DOMAINS);
    }

    #[test]
    fn parse_multilist() {
        let expectation = EASY_LIST + EASY_PRIVACY + UBLOCK_UNBREAK + BRAVE_UNBREAK;
        check_list_counts(
            &vec![
                String::from("./data/test/easylist.txt"),
                String::from("./data/test/easyprivacy.txt"),
                String::from("./data/test/ublock-unbreak.txt"),
                String::from("./data/test/brave-unbreak.txt"),
            ],
            FilterFormat::Standard,
            expectation,
        )
    }

    #[test]
    fn parse_malware_multilist() {
        let expectation = SPAM_404_MAIN_BLACKLIST + DISCONNECT_SIMPLE_MALWARE;
        check_list_counts(
            &vec![
                String::from("./data/test/spam404-main-blacklist.txt"),
                String::from("./data/test/disconnect-simple-malware.txt"),
            ],
            FilterFormat::Standard,
            expectation,
        )
    }

    #[test]
    fn parse_hosts_formats() {
        let mut expectation = MALWARE_DOMAIN_LIST + MALWARE_DOMAINS;
        expectation.duplicates = 69;
        check_list_counts(
            &vec![
                String::from("./data/test/malwaredomainlist.txt"),
                String::from("./data/test/malwaredomains.txt"),
            ],
            FilterFormat::Hosts,
            expectation,
        )
    }
}
