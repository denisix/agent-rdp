//! Command implementations.

use agent_rdp_protocol::Region;

/// Parse a `--region X,Y,WIDTH,HEIGHT` argument.
///
/// Shared by `screenshot` and `locate` so both accept exactly the same syntax.
pub fn parse_region(s: &str) -> Result<Region, String> {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    if parts.len() != 4 {
        return Err(format!(
            "expected X,Y,WIDTH,HEIGHT (4 comma-separated values), got {}",
            parts.len()
        ));
    }

    let mut values = [0u32; 4];
    for (i, part) in parts.iter().enumerate() {
        values[i] = part
            .parse::<u32>()
            .map_err(|_| format!("'{}' is not a non-negative integer", part))?;
    }

    let [x, y, width, height] = values;
    if width == 0 || height == 0 {
        return Err("width and height must be greater than zero".to_string());
    }

    Ok(Region { x, y, width, height })
}

/// Parse a `--window WIDTHxHEIGHT` argument (e.g. `400x160`).
pub fn parse_window(s: &str) -> Result<(u32, u32), String> {
    let parts: Vec<&str> = s.split('x').map(str::trim).collect();
    if parts.len() != 2 {
        return Err(format!(
            "expected WIDTHxHEIGHT (e.g. 400x160), got '{}'",
            s
        ));
    }

    let width: u32 = parts[0]
        .parse()
        .map_err(|_| format!("'{}' is not a non-negative integer", parts[0]))?;
    let height: u32 = parts[1]
        .parse()
        .map_err(|_| format!("'{}' is not a non-negative integer", parts[1]))?;

    if width == 0 || height == 0 {
        return Err("width and height must be greater than zero".to_string());
    }

    Ok((width, height))
}

/// Parse a `--confirm X,Y` argument: a second independently measured point
/// for the same target, used by `click-at`'s cross-check.
pub fn parse_point(s: &str) -> Result<(u16, u16), String> {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    if parts.len() != 2 {
        return Err(format!("expected X,Y (2 comma-separated values), got '{}'", s));
    }

    let x: u16 = parts[0]
        .parse()
        .map_err(|_| format!("'{}' is not a valid coordinate", parts[0]))?;
    let y: u16 = parts[1]
        .parse()
        .map_err(|_| format!("'{}' is not a valid coordinate", parts[1]))?;

    Ok((x, y))
}

pub mod automate;
pub mod clipboard;
pub mod connect;
pub mod diagnose;
pub mod disconnect;
pub mod drive;
pub mod file;
pub mod keyboard;
pub mod locate;
pub mod mouse;
pub mod screenshot;
pub mod scroll;
pub mod session;
pub mod view;
pub mod wait;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_region_valid() {
        assert_eq!(
            parse_region("100,380,400,30"),
            Ok(Region { x: 100, y: 380, width: 400, height: 30 })
        );
        // Surrounding whitespace is tolerated.
        assert_eq!(
            parse_region(" 0, 0 , 1280,800 "),
            Ok(Region { x: 0, y: 0, width: 1280, height: 800 })
        );
    }

    #[test]
    fn test_parse_region_wrong_arity() {
        assert!(parse_region("100,380,400").is_err());
        assert!(parse_region("100,380,400,30,5").is_err());
        assert!(parse_region("").is_err());
    }

    #[test]
    fn test_parse_region_rejects_non_numeric_and_negative() {
        assert!(parse_region("100,380,400,thirty").is_err());
        assert!(parse_region("-1,380,400,30").is_err());
    }

    #[test]
    fn test_parse_region_rejects_empty_area() {
        // A zero-area region would silently return an empty image.
        assert!(parse_region("100,380,0,30").is_err());
        assert!(parse_region("100,380,400,0").is_err());
    }

    #[test]
    fn test_parse_region_at_origin_and_single_pixel() {
        // Both extremes of a legitimate region.
        assert_eq!(
            parse_region("0,0,1,1"),
            Ok(Region { x: 0, y: 0, width: 1, height: 1 })
        );
    }

    #[test]
    fn test_parse_region_accepts_large_coordinates() {
        // Out-of-bounds is the daemon's call, not the parser's - it only knows
        // the syntax, so a plausible 8K coordinate must parse fine.
        assert_eq!(
            parse_region("7680,4320,100,100"),
            Ok(Region { x: 7680, y: 4320, width: 100, height: 100 })
        );
    }

    #[test]
    fn test_parse_region_rejects_empty_components() {
        assert!(parse_region("1,,3,4").is_err());
        assert!(parse_region(",,,").is_err());
        assert!(parse_region("100,380,400,").is_err());
    }

    #[test]
    fn test_parse_region_rejects_overflowing_values() {
        // Larger than u32 must be an error, not a wrapped-around coordinate.
        assert!(parse_region("4294967296,0,10,10").is_err());
    }

    #[test]
    fn test_parse_region_rejects_wrong_separators() {
        assert!(parse_region("100 380 400 30").is_err());
        assert!(parse_region("100x380x400x30").is_err());
        assert!(parse_region("100;380;400;30").is_err());
    }

    #[test]
    fn test_parse_region_rejects_floats() {
        // A float would otherwise silently truncate to a different pixel.
        assert!(parse_region("100.5,380,400,30").is_err());
    }

    #[test]
    fn test_parse_region_error_mentions_the_expected_shape() {
        // The message is the only guidance a caller gets, so it has to name
        // the format rather than just saying "invalid".
        let err = parse_region("1,2,3").unwrap_err();
        assert!(err.contains("X,Y,WIDTH,HEIGHT"), "unhelpful message: {}", err);
    }

    #[test]
    fn test_parse_window_valid() {
        assert_eq!(parse_window("400x160"), Ok((400, 160)));
        assert_eq!(parse_window(" 400 x 160 "), Ok((400, 160)));
        assert_eq!(parse_window("1x1"), Ok((1, 1)));
    }

    #[test]
    fn test_parse_window_rejects_bad_input() {
        assert!(parse_window("400").is_err());
        assert!(parse_window("400x160x2").is_err());
        assert!(parse_window("400,160").is_err());
        assert!(parse_window("0x160").is_err());
        assert!(parse_window("400x0").is_err());
        assert!(parse_window("wxh").is_err());
        assert!(parse_window("").is_err());
    }

    #[test]
    fn test_parse_point_valid() {
        assert_eq!(parse_point("665,209"), Ok((665, 209)));
        assert_eq!(parse_point(" 665 , 209 "), Ok((665, 209)));
        assert_eq!(parse_point("0,0"), Ok((0, 0)));
    }

    #[test]
    fn test_parse_point_rejects_bad_input() {
        assert!(parse_point("665").is_err());
        assert!(parse_point("665,209,1").is_err());
        assert!(parse_point("665x209").is_err());
        assert!(parse_point("-1,209").is_err());
        assert!(parse_point("").is_err());
    }
}
