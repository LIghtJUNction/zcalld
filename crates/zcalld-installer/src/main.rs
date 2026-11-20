use std::process::ExitCode;
fn main()->ExitCode{
    let args=std::env::args().skip(1).collect::<Vec<_>>();
    if args==["--version"]||args==["-V"]{println!("zcalld-installer {}",env!("CARGO_PKG_VERSION"));return ExitCode::SUCCESS;}
    if args==["--help"]||args==["-h"]{print!("{}",zcalld_installer::cli::HELP);return ExitCode::SUCCESS;}
    match zcalld_installer::cli::run(&args){
        Ok(value)=>{println!("{}",value.stringify());ExitCode::SUCCESS}
        Err(error)=>{println!("{}",zcalld_installer::json::object([("ok",false.into()),("error",error.into())]).stringify());ExitCode::FAILURE}
    }
}
