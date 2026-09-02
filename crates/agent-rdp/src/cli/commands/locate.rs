//! Locate command implementation (OCR-based text location).

use agent_rdp_protocol::{
    ClickAtRequest, LocateRequest, MouseRequest, OcrMatch, Request, ResponseData,
};

use crate::cli::{ClickAtArgs, LocateArgs};
use crate::output::Output;
use crate::session_manager::SessionManager;

/// How a located match should be clicked, if at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClickKind {
    Left,
    Double,
    Right,
}

impl ClickKind {
    fn from_args(args: &LocateArgs) -> Option<Self> {
        if args.double_click {
            Some(Self::Double)
        } else if args.right_click {
            Some(Self::Right)
        } else if args.click {
            Some(Self::Left)
        } else {
            None
        }
    }

    fn request(self, x: u16, y: u16) -> MouseRequest {
        match self {
            Self::Left => MouseRequest::Click { x, y },
            Self::Double => MouseRequest::DoubleClick { x, y },
            Self::Right => MouseRequest::RightClick { x, y },
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Left => "Clicked",
            Self::Double => "Double-clicked",
            Self::Right => "Right-clicked",
        }
    }
}

/// Pick the single match to click, or explain why the choice is ambiguous.
///
/// Clicking the wrong one of several identically-named controls is worse than
/// not clicking at all, so an ambiguous request is an error rather than a guess
/// at the first match.
fn select_match<'a>(matches: &'a [OcrMatch], index: Option<usize>) -> Result<&'a OcrMatch, String> {
    match index {
        Some(i) => matches.get(i).ok_or_else(|| {
            format!("--index {} is out of range: only {} match(es) found", i, matches.len())
        }),
        None if matches.len() == 1 => Ok(&matches[0]),
        None => Err(format!(
            "{} matches found - pass --index to choose one, or narrow the search with --region",
            matches.len()
        )),
    }
}

/// Convert an OCR centre point into clickable screen coordinates.
fn click_point(m: &OcrMatch) -> Result<(u16, u16), String> {
    let to_u16 = |v: i32, axis: &str| -> Result<u16, String> {
        u16::try_from(v).map_err(|_| format!("match {} coordinate {} is off-screen", axis, v))
    };
    Ok((to_u16(m.center_x, "x")?, to_u16(m.center_y, "y")?))
}

/// Build the "no matches" message, with an extra hint when `--near` was set:
/// zero matches there could mean the anchor itself wasn't found, not that
/// the query text is absent, and those need different fixes.
fn no_match_message(search_text: &str, total_lines: u32, near: &Option<String>) -> String {
    let base = format!("No lines containing '{}' found ({} lines detected)", search_text, total_lines);
    match near {
        Some(anchor) => format!(
            "{}. This could also mean the --near anchor '{}' itself wasn't found - try \
             `locate '{}'` (without --near) to check.",
            base, anchor, anchor
        ),
        None => base,
    }
}

pub async fn run(
    session: &str,
    args: LocateArgs,
    output: &Output,
    timeout_ms: u64,
) -> anyhow::Result<()> {
    let manager = SessionManager::new(session.to_string());

    let mut client = match manager.connect_existing().await {
        Ok(client) => client,
        Err(unavailable) => {
            output.print_error(unavailable.code(), unavailable.message());
            std::process::exit(1);
        }
    };

    let search_text = args.text.clone().unwrap_or_default();

    let click_kind = ClickKind::from_args(&args);

    let request = Request::Locate(LocateRequest {
        text: search_text.clone(),
        pattern: args.pattern,
        exact: args.exact,
        ignore_case: !args.case_sensitive,
        all: args.all,
        region: args.region,
        wait_ms: args.wait,
        near: args.near.clone(),
        near_distance: args.near_distance,
    });

    // The daemon polls for up to `--wait` milliseconds before answering, so
    // the IPC timeout has to cover that plus normal round-trip slack -
    // otherwise the CLI would time out its own client while the daemon was
    // still legitimately waiting.
    let locate_timeout_ms = args.wait.map_or(timeout_ms, |wait_ms| wait_ms + timeout_ms);

    let response = client.send(&request, locate_timeout_ms).await?;

    if !response.success {
        output.print_response(&response);
        std::process::exit(1);
    }

    // Handle the locate result
    if let Some(ResponseData::LocateResult(result)) = response.data {
        // Click mode: resolve the target and click it here, so the coordinate
        // never has to be read off a screenshot and typed back in.
        if let Some(kind) = click_kind {
            if result.matches.is_empty() {
                output.print_error("no_match", &no_match_message(&search_text, result.total_words, &args.near));
                std::process::exit(1);
            }

            let target = match select_match(&result.matches, args.index) {
                Ok(m) => m,
                Err(msg) => {
                    output.print_error("ambiguous_match", &msg);
                    if !output.is_json() {
                        for (i, m) in result.matches.iter().enumerate() {
                            eprintln!("  [{}] '{}' - center: ({}, {})", i, m.text, m.center_x, m.center_y);
                        }
                    }
                    std::process::exit(1);
                }
            };

            let (x, y) = match click_point(target) {
                Ok(point) => point,
                Err(msg) => {
                    output.print_error("invalid_coordinates", &msg);
                    std::process::exit(1);
                }
            };

            let click_response = client.send(&Request::Mouse(kind.request(x, y)), timeout_ms).await?;
            if !click_response.success {
                output.print_response(&click_response);
                std::process::exit(1);
            }

            if output.is_json() {
                println!("{}", serde_json::to_string(&serde_json::json!({
                    "success": true,
                    "data": {
                        "type": "locate_click",
                        "text": target.text,
                        "x": x,
                        "y": y
                    }
                }))?);
            } else {
                println!("{} '{}' at ({}, {})", kind.label(), target.text, x, y);
            }

            return Ok(());
        }

        if output.is_json() {
            // Output full JSON result
            println!("{}", serde_json::to_string(&serde_json::json!({
                "success": true,
                "data": {
                    "matches": result.matches,
                    "total_lines": result.total_words
                }
            }))?);
        } else if args.all {
            // Show all lines
            println!("Found {} text lines on screen:", result.matches.len());
            for m in &result.matches {
                println!("  '{}' at ({}, {}) size {}x{} - center: ({}, {})",
                    m.text, m.x, m.y, m.width, m.height, m.center_x, m.center_y);
            }
        } else {
            // Search mode
            if result.matches.is_empty() {
                println!("{}", no_match_message(&search_text, result.total_words, &args.near));
            } else {
                println!("Found {} line(s) containing '{}' ({} lines detected):",
                    result.matches.len(), search_text, result.total_words);
                for m in &result.matches {
                    println!("  '{}' at ({}, {}) size {}x{} - center: ({}, {})",
                        m.text, m.x, m.y, m.width, m.height, m.center_x, m.center_y);
                }
                // Point at --click rather than at hand-copied coordinates:
                // re-typing a coordinate is where clicks go wrong.
                if result.matches.len() == 1 {
                    println!("\nTo click it: agent-rdp locate '{}' --click", search_text);
                } else {
                    println!("\nTo click one: agent-rdp locate '{}' --click --index N", search_text);
                }
            }
        }
    }

    Ok(())
}

/// Run the `click-at` command: click a caller-supplied point with a
/// geometric ambiguity check.
pub async fn run_click_at(
    session: &str,
    args: ClickAtArgs,
    output: &Output,
    timeout_ms: u64,
) -> anyhow::Result<()> {
    let manager = SessionManager::new(session.to_string());

    let mut client = match manager.connect_existing().await {
        Ok(client) => client,
        Err(unavailable) => {
            output.print_error(unavailable.code(), unavailable.message());
            std::process::exit(1);
        }
    };

    let (window_width, window_height) = args.window.unwrap_or((400, 160));

    let (confirm_x, confirm_y) = match args.confirm {
        Some((x, y)) => (Some(x), Some(y)),
        None => (None, None),
    };

    let request = Request::ClickAt(ClickAtRequest {
        x: args.x,
        y: args.y,
        window_width,
        window_height,
        min_gap: args.min_gap.unwrap_or(10),
        double_click: args.double_click,
        right_click: args.right_click,
        confirm_x,
        confirm_y,
        max_divergence: args.max_divergence.unwrap_or(40),
    });

    let response = client.send(&request, timeout_ms).await?;

    if !response.success {
        output.print_response(&response);
        std::process::exit(1);
    }

    if let Some(ResponseData::ClickAtResult(result)) = &response.data {
        if output.is_json() {
            output.print_response(&response);
        } else if result.clicked {
            match &result.matched_text {
                Some(text) => println!(
                    "Clicked at ({}, {}) - OCR read '{}' nearby (best-effort; recognition \
                     may be wrong for text OCR struggles with)",
                    result.x, result.y, text
                ),
                None => println!(
                    "Clicked at ({}, {}) - no text detected nearby",
                    result.x, result.y
                ),
            }
        } else if let Some(divergence) = result.divergence {
            output.print_error(
                "measurements_diverge",
                &format!(
                    "confirm point diverges {}px from ({}, {}), past --max-divergence {}px - \
                     the two measurements don't agree closely enough to trust either",
                    divergence,
                    result.x,
                    result.y,
                    args.max_divergence.unwrap_or(40)
                ),
            );
            std::process::exit(1);
        } else {
            // Refused: mirror the ambiguous-match formatting `locate --click`
            // uses, so the two commands read the same way when they decline.
            output.print_error(
                "ambiguous_click",
                &format!(
                    "{} text regions detected near ({}, {}) - move the point or pass a \
                     smaller --min-gap",
                    result.nearby.len(),
                    result.x,
                    result.y
                ),
            );
            for (i, m) in result.nearby.iter().enumerate() {
                eprintln!(
                    "  [{}] '{}' at ({}, {}) size {}x{}",
                    i, m.text, m.x, m.y, m.width, m.height
                );
            }
            std::process::exit(1);
        }
        if !result.clicked {
            std::process::exit(1);
        }
        return Ok(());
    }

    output.print_response(&response);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(text: &str, center_x: i32, center_y: i32) -> OcrMatch {
        OcrMatch {
            text: text.to_string(),
            x: center_x - 20,
            y: center_y - 6,
            width: 40,
            height: 12,
            center_x,
            center_y,
        }
    }

    #[test]
    fn test_select_match_single() {
        let matches = vec![m("Добавить", 612, 300)];
        assert_eq!(select_match(&matches, None).unwrap().center_y, 300);
    }

    #[test]
    fn test_select_match_ambiguous_is_an_error() {
        // Two "Добавить" buttons: guessing the first one is how a click lands
        // on the wrong toolbar.
        let matches = vec![m("Добавить", 612, 300), m("Добавить", 612, 500)];
        assert!(select_match(&matches, None).is_err());
    }

    #[test]
    fn test_select_match_by_index() {
        let matches = vec![m("Добавить", 612, 300), m("Добавить", 612, 500)];
        assert_eq!(select_match(&matches, Some(1)).unwrap().center_y, 500);
        assert!(select_match(&matches, Some(2)).is_err());
    }

    #[test]
    fn test_click_point_rejects_offscreen() {
        assert_eq!(click_point(&m("OK", 612, 300)).unwrap(), (612, 300));
        assert!(click_point(&m("OK", -5, 300)).is_err());
        assert!(click_point(&m("OK", 612, 70_000)).is_err());
    }

    #[test]
    fn test_select_match_index_zero_on_a_single_match() {
        // An explicit --index 0 must behave the same as the implicit case.
        let matches = vec![m("Добавить", 612, 300)];
        assert_eq!(select_match(&matches, Some(0)).unwrap().center_y, 300);
    }

    #[test]
    fn test_select_match_on_empty_input_is_an_error() {
        // The caller checks for emptiness first, but this must not panic if
        // that check is ever moved or removed.
        let matches: Vec<OcrMatch> = Vec::new();
        assert!(select_match(&matches, None).is_err());
        assert!(select_match(&matches, Some(0)).is_err());
    }

    #[test]
    fn test_select_match_error_lists_the_count() {
        // The message has to tell the caller how many candidates there were,
        // otherwise --index is guesswork.
        let matches = vec![m("Добавить", 612, 300), m("Добавить", 612, 500)];
        let err = select_match(&matches, None).unwrap_err();
        assert!(err.contains('2'), "should report the match count: {}", err);
        assert!(err.contains("--index"), "should point at the fix: {}", err);

        let err = select_match(&matches, Some(9)).unwrap_err();
        assert!(err.contains('9') && err.contains('2'), "unhelpful message: {}", err);
    }

    #[test]
    fn test_select_match_picks_the_last_index() {
        let matches = vec![m("a", 1, 1), m("b", 2, 2), m("c", 3, 3)];
        assert_eq!(select_match(&matches, Some(2)).unwrap().text, "c");
        assert!(select_match(&matches, Some(3)).is_err());
    }

    #[test]
    fn test_click_point_accepts_the_coordinate_extremes() {
        // 0 and u16::MAX are both valid MousePdu coordinates; one past the top
        // is not.
        assert_eq!(click_point(&m("OK", 0, 0)).unwrap(), (0, 0));
        assert_eq!(click_point(&m("OK", 65_535, 65_535)).unwrap(), (65_535, 65_535));
        assert!(click_point(&m("OK", 65_536, 0)).is_err());
        assert!(click_point(&m("OK", 0, -1)).is_err());
    }

    #[test]
    fn test_click_point_names_the_offending_axis() {
        let err = click_point(&m("OK", -5, 300)).unwrap_err();
        assert!(err.contains('x'), "should name the axis: {}", err);

        let err = click_point(&m("OK", 300, -5)).unwrap_err();
        assert!(err.contains('y'), "should name the axis: {}", err);
    }

    #[test]
    fn test_click_kind_precedence_and_absence() {
        // clap keeps these mutually exclusive, but the mapping still has to be
        // right for each one on its own - and absent means "do not click".
        let base = |click, double_click, right_click| LocateArgs {
            text: Some("x".to_string()),
            pattern: false,
            exact: false,
            case_sensitive: false,
            all: false,
            region: None,
            wait: None,
            click,
            double_click,
            right_click,
            index: None,
            near: None,
            near_distance: 150,
        };

        assert_eq!(ClickKind::from_args(&base(false, false, false)), None);
        assert_eq!(ClickKind::from_args(&base(true, false, false)), Some(ClickKind::Left));
        assert_eq!(ClickKind::from_args(&base(false, true, false)), Some(ClickKind::Double));
        assert_eq!(ClickKind::from_args(&base(false, false, true)), Some(ClickKind::Right));
    }

    #[test]
    fn test_click_kind_maps_to_the_matching_request() {
        // A left click that silently sent a right click would be a nasty bug.
        assert!(matches!(
            ClickKind::Left.request(10, 20),
            MouseRequest::Click { x: 10, y: 20 }
        ));
        assert!(matches!(
            ClickKind::Double.request(10, 20),
            MouseRequest::DoubleClick { x: 10, y: 20 }
        ));
        assert!(matches!(
            ClickKind::Right.request(10, 20),
            MouseRequest::RightClick { x: 10, y: 20 }
        ));
    }
}
