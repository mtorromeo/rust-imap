use imap_proto::{self, MailboxDatum, Response};
use nom::IResult;
use regex::Regex;
use std::str;

use super::error::{Error, ParseError, Result};
use super::types::*;

pub fn parse_authenticate_response(line: String) -> Result<String> {
    let authenticate_regex = Regex::new("^+(.*)\r\n").unwrap();

    if let Some(cap) = authenticate_regex.captures_iter(line.as_str()).next() {
        let data = cap.get(1).map(|x| x.as_str()).unwrap_or("");
        return Ok(String::from(data));
    }

    Err(Error::Parse(ParseError::Authentication(line)))
}

enum MapOrNot<T> {
    Map(T),
    Not(Response<'static>),
    #[allow(dead_code)]
    Ignore,
}

unsafe fn parse_many<T, F>(lines: Vec<u8>, mut map: F) -> ZeroCopyResult<Vec<T>>
where
    F: FnMut(Response<'static>) -> MapOrNot<T>,
{
    let f = |mut lines: &'static [u8]| {
        let mut things = Vec::new();
        loop {
            if lines.is_empty() {
                break Ok(things);
            }

            match imap_proto::parse_response(lines) {
                IResult::Done(rest, resp) => {
                    lines = rest;

                    match map(resp) {
                        MapOrNot::Map(t) => things.push(t),
                        MapOrNot::Not(resp) => {
                            // check if this is simply a unilateral server response
                            // (see Section 7 of RFC 3501):
                            match resp {
                                Response::MailboxData(MailboxDatum::Recent { .. })
                                | Response::MailboxData(MailboxDatum::Exists { .. })
                                | Response::Fetch(..)
                                | Response::Expunge(..) => {
                                    continue;
                                }
                                resp => break Err(resp.into()),
                            }
                        }
                        MapOrNot::Ignore => continue,
                    }
                }
                _ => {
                    break Err(Error::Parse(ParseError::Invalid(lines.to_vec())));
                }
            }
        }
    };

    ZeroCopy::new(lines, f)
}

pub fn parse_names(lines: Vec<u8>) -> ZeroCopyResult<Vec<Name>> {
    use imap_proto::MailboxDatum;
    let f = |resp| match resp {
        // https://github.com/djc/imap-proto/issues/4
        Response::MailboxData(MailboxDatum::List {
            flags,
            delimiter,
            name,
        })
        | Response::MailboxData(MailboxDatum::SubList {
            flags,
            delimiter,
            name,
        }) => MapOrNot::Map(Name {
            attributes: flags,
            delimiter,
            name,
        }),
        resp => MapOrNot::Not(resp),
    };

    unsafe { parse_many(lines, f) }
}

pub fn parse_fetches(lines: Vec<u8>) -> ZeroCopyResult<Vec<Fetch>> {
    let f = |resp| match resp {
        Response::Fetch(num, attrs) => {
            let mut fetch = Fetch {
                message: num,
                flags: vec![],
                uid: None,
                rfc822_header: None,
                rfc822: None,
                body: None,
            };

            for attr in attrs {
                use imap_proto::AttributeValue;
                match attr {
                    AttributeValue::Flags(flags) => {
                        fetch.flags.extend(flags);
                    }
                    AttributeValue::Uid(uid) => fetch.uid = Some(uid),
                    AttributeValue::Rfc822(rfc) => fetch.rfc822 = rfc,
                    AttributeValue::Rfc822Header(rfc) => fetch.rfc822_header = rfc,
                    AttributeValue::BodySection {
                        data, ..
                    } => fetch.body = data,
                    _ => {}
                }
            }

            MapOrNot::Map(fetch)
        }
        resp => MapOrNot::Not(resp),
    };

    unsafe { parse_many(lines, f) }
}

pub fn parse_capabilities(lines: Vec<u8>) -> ZeroCopyResult<Capabilities> {
    let f = |mut lines| {
        use std::collections::HashSet;
        let mut caps = HashSet::new();
        loop {
            match imap_proto::parse_response(lines) {
                IResult::Done(rest, Response::Capabilities(c)) => {
                    lines = rest;
                    caps.extend(c);

                    if lines.is_empty() {
                        break Ok(Capabilities(caps));
                    }
                }
                IResult::Done(_, resp) => {
                    break Err(resp.into());
                }
                _ => {
                    break Err(Error::Parse(ParseError::Invalid(lines.to_vec())));
                }
            }
        }
    };

    unsafe { ZeroCopy::new(lines, f) }
}

pub fn parse_mailbox(mut lines: &[u8]) -> Result<Mailbox> {
    let mut mailbox = Mailbox::default();

    loop {
        match imap_proto::parse_response(lines) {
            IResult::Done(rest, Response::Data { status, code, .. }) => {
                lines = rest;

                if let imap_proto::Status::Ok = status {
                } else {
                    // how can this happen for a Response::Data?
                    unreachable!();
                }

                use imap_proto::ResponseCode;
                match code {
                    Some(ResponseCode::UidValidity(uid)) => {
                        mailbox.uid_validity = Some(uid);
                    }
                    Some(ResponseCode::UidNext(unext)) => {
                        mailbox.uid_next = Some(unext);
                    }
                    Some(ResponseCode::Unseen(n)) => {
                        mailbox.unseen = Some(n);
                    }
                    Some(ResponseCode::PermanentFlags(flags)) => {
                        mailbox
                            .permanent_flags
                            .extend(flags.into_iter().map(|s| s.to_string()));
                    }
                    _ => {}
                }
            }
            IResult::Done(rest, Response::MailboxData(m)) => {
                lines = rest;

                use imap_proto::MailboxDatum;
                match m {
                    MailboxDatum::Status { .. } => {
                        // TODO: we probably want to expose statuses too
                    }
                    MailboxDatum::Exists(e) => {
                        mailbox.exists = e;
                    }
                    MailboxDatum::Recent(r) => {
                        mailbox.recent = r;
                    }
                    MailboxDatum::Flags(flags) => {
                        mailbox
                            .flags
                            .extend(flags.into_iter().map(|s| s.to_string()));
                    }
                    MailboxDatum::SubList { .. } | MailboxDatum::List { .. } => {}
                }
            }
            IResult::Done(_, resp) => {
                break Err(resp.into());
            }
            _ => {
                break Err(Error::Parse(ParseError::Invalid(lines.to_vec())));
            }
        }

        if lines.is_empty() {
            break Ok(mailbox);
        }
    }
}

pub fn parse_search_ids(lines: &[u8]) -> Result<Vec<u32>> {
    match str::from_utf8(lines) {
        Ok(resp) => {
            let re = Regex::new(r"^(?i)\* SEARCH((?:\s+[0-9]+)*)\s*$").unwrap();
            match re.captures(resp) {
                Some(cap) => {
                    let mut line = cap.get(1).map(|m| m.as_str()).unwrap_or("");
                    Ok(line
                        .trim()
                        .split(' ')
                        .filter_map(|id| id.trim().parse().ok())
                        .collect())
                }
                None => Err(Error::Parse(ParseError::Invalid(lines.to_vec()))),
            }
        }
        Err(_) => Err(Error::Parse(ParseError::Invalid(lines.to_vec()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_capability_test() {
        let expected_capabilities = vec!["IMAP4rev1", "STARTTLS", "AUTH=GSSAPI", "LOGINDISABLED"];
        let lines = b"* CAPABILITY IMAP4rev1 STARTTLS AUTH=GSSAPI LOGINDISABLED\r\n";
        let capabilities = parse_capabilities(lines.to_vec()).unwrap();
        assert_eq!(capabilities.len(), 4);
        for e in expected_capabilities {
            assert!(capabilities.has(e));
        }
    }

    #[test]
    #[should_panic]
    fn parse_capability_invalid_test() {
        let lines = b"* JUNK IMAP4rev1 STARTTLS AUTH=GSSAPI LOGINDISABLED\r\n";
        parse_capabilities(lines.to_vec()).unwrap();
    }

    #[test]
    fn parse_names_test() {
        let lines = b"* LIST (\\HasNoChildren) \".\" \"INBOX\"\r\n";
        let names = parse_names(lines.to_vec()).unwrap();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].attributes(), &["\\HasNoChildren"]);
        assert_eq!(names[0].delimiter(), ".");
        assert_eq!(names[0].name(), "INBOX");
    }

    #[test]
    fn parse_fetches_empty() {
        let lines = b"";
        let fetches = parse_fetches(lines.to_vec()).unwrap();
        assert!(fetches.is_empty());
    }

    #[test]
    fn parse_fetches_test() {
        let lines = b"\
                    * 24 FETCH (FLAGS (\\Seen) UID 4827943)\r\n\
                    * 25 FETCH (FLAGS (\\Seen))\r\n";
        let fetches = parse_fetches(lines.to_vec()).unwrap();
        assert_eq!(fetches.len(), 2);
        assert_eq!(fetches[0].message, 24);
        assert_eq!(fetches[0].flags(), &["\\Seen"]);
        assert_eq!(fetches[0].uid, Some(4827943));
        assert_eq!(fetches[0].rfc822(), None);
        assert_eq!(fetches[1].message, 25);
        assert_eq!(fetches[1].flags(), &["\\Seen"]);
        assert_eq!(fetches[1].uid, None);
        assert_eq!(fetches[1].rfc822(), None);
    }

    #[test]
    fn parse_fetches_w_unilateral() {
        // https://github.com/mattnenterprise/rust-imap/issues/81
        let lines = b"\
            * 37 FETCH (UID 74)\r\n\
            * 1 RECENT\r\n";
        let fetches = parse_fetches(lines.to_vec()).unwrap();
        assert_eq!(fetches.len(), 1);
        assert_eq!(fetches[0].message, 37);
        assert_eq!(fetches[0].uid, Some(74));
    }

    #[test]
    fn parse_search_ids_test() {
        let lines = "* SEARCH 1600 1698 1739 1781 1795 1885 1891 1892 1893 1898 1899 1901 1911 1926 1932 1933 1993 1994 2007 2032 2033 2041 2053 2062 2063 2065 2066 2072 2078 2079 2082 2084 2095 2100 2101 2102 2103 2104 2107 2116 2120 2135 2138 2154 2163 2168 2172 2189 2193 2198 2199 2205 2212 2213 2221 2227 2267 2275 2276 2295 2300 2328 2330 2332 2333 2334 2335 2336 2337 2338 2339 2341 2342 2347 2349 2350 2358 2359 2362 2369 2371 2372 2373 2374 2375 2376 2377 2378 2379 2380 2381 2382 2383 2384 2385 2386 2390 2392 2397 2400 2401 2403 2405 2409 2411 2414 2417 2419 2420 2424 2426 2428 2439 2454 2456 2467 2468 2469 2490 2515 2519 2520 2521".as_bytes();
        assert_eq!(
            parse_search_ids(lines).unwrap(),
            vec![
                1600, 1698, 1739, 1781, 1795, 1885, 1891, 1892, 1893, 1898, 1899, 1901, 1911, 1926,
                1932, 1933, 1993, 1994, 2007, 2032, 2033, 2041, 2053, 2062, 2063, 2065, 2066, 2072,
                2078, 2079, 2082, 2084, 2095, 2100, 2101, 2102, 2103, 2104, 2107, 2116, 2120, 2135,
                2138, 2154, 2163, 2168, 2172, 2189, 2193, 2198, 2199, 2205, 2212, 2213, 2221, 2227,
                2267, 2275, 2276, 2295, 2300, 2328, 2330, 2332, 2333, 2334, 2335, 2336, 2337, 2338,
                2339, 2341, 2342, 2347, 2349, 2350, 2358, 2359, 2362, 2369, 2371, 2372, 2373, 2374,
                2375, 2376, 2377, 2378, 2379, 2380, 2381, 2382, 2383, 2384, 2385, 2386, 2390, 2392,
                2397, 2400, 2401, 2403, 2405, 2409, 2411, 2414, 2417, 2419, 2420, 2424, 2426, 2428,
                2439, 2454, 2456, 2467, 2468, 2469, 2490, 2515, 2519, 2520, 2521
            ]
        );

        let lines = "* SEARCH".as_bytes();
        assert_eq!(parse_search_ids(lines).unwrap(), Vec::<u32>::new());
    }
}
