//! Native adapter for explicit input from one local SSMCP listener.
#[cfg(target_os = "linux")]
mod local;

pub fn run(args: &[String]) -> Option<Result<(), String>> {
    if args.first().is_none_or(|command| command != "listen") {
        return None;
    }
    #[cfg(target_os = "linux")]
    return Some(local::run(&args[1..]));
    #[cfg(not(target_os = "linux"))]
    Some(Err(
        "zcalld listen requires the Linux private SSMCP socket".into()
    ))
}
