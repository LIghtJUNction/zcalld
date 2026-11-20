use crate::{json::{self,Value},Plan};
use std::{collections::BTreeMap,path::{Path,PathBuf},time::Duration};

pub const HELP:&str="zcalld-installer plan [--version X.Y.Z] [--prefix PATH] --out PLAN\n\
  plan --archive FILE --sha256 HEX --version X.Y.Z --source COMMIT [--prefix PATH] --out PLAN\n\
  apply --plan PLAN [--defer]\n\
  status|rollback --transaction PATH\n\
  resume --transaction PATH [--wait-ms 0..120000]\n\
  defaults\n\
All operations return JSON. Local archives are explicitly pinned local inputs;\n\
only plans obtained from the official release API claim GitHub provenance.\n\
No operation changes agent configuration, starts a service, or kills a gateway.\n";

pub fn run(args:&[String])->Result<Value,String>{
    let Some(command)=args.first().map(String::as_str)else{return Err(HELP.into());};
    let accepted:&[&str]=match command{
        "plan"=>&["version","prefix","out","archive","sha256","source"],
        "apply"=>&["plan","defer"],"status"|"rollback"=>&["transaction"],"resume"=>&["transaction","wait-ms"],"defaults"=>&[],
        _=>return Err("unknown installer command".into()),
    };
    let mut values=BTreeMap::new();let mut i=1;let mut json_flag=false;
    while i<args.len(){let Some(flag)=args[i].strip_prefix("--")else{return Err("unexpected installer argument".into());};i+=1;
        if flag=="json"{if json_flag{return Err("duplicate --json".into());}json_flag=true;continue;}
        if !accepted.contains(&flag)||values.contains_key(flag){return Err(format!("unknown or duplicate installer option: --{flag}"));}
        let value=if flag=="defer"{"true".to_owned()}else{let v=args.get(i).filter(|s|!s.is_empty()&&!s.starts_with("--")).ok_or_else(||format!("--{flag} requires a value"))?;i+=1;v.clone()};
        values.insert(flag,value);
    }
    let need=|key:&str|values.get(key).map(String::as_str).ok_or_else(||format!("missing --{key}"));
    match command{
        "defaults"=>Ok(json::object([("ok",true.into()),("schema",1usize.into()),("version",env!("CARGO_PKG_VERSION").into()),("target",crate::host_target().into()),
            ("prefix",crate::default_prefix()?.to_string_lossy().into_owned().into()),("protocol",1usize.into()),("full_archive",true.into()),("runtime_lease",true.into())])),
        "plan"=>{
            let requested=values.get("version").map(String::as_str);let prefix=values.get("prefix").map(PathBuf::from);
            let plan=if let Some(path)=values.get("archive"){
                Plan::local(Path::new(path),need("sha256")?,need("version")?,need("source")?,prefix.as_deref().unwrap_or(&crate::default_prefix()?))?
            }else{if values.contains_key("sha256")||values.contains_key("source"){return Err("local digest and source require --archive".into());}
                crate::plan_official(requested,prefix.as_deref())?};
            let out=Path::new(need("out")?);plan.save(out)?;let mut result=plan.value();result.set("ok",true.into());result.set("plan_file",out.to_string_lossy().into_owned().into());result.set("phase","planned".into());Ok(result)
        }
        "apply"=>{let result=crate::apply(&Plan::load(Path::new(need("plan")?))?)?;
            if values.contains_key("defer")&&result.get("pending_exit").as_bool()==Some(true){crate::spawn_resume(Path::new(result.get("transaction").as_str().ok_or("missing transaction result")?))}else{Ok(result)}},
        "status"=>crate::status(Path::new(need("transaction")?)),
        "rollback"=>crate::rollback(Path::new(need("transaction")?)),
        "resume"=>{let wait=values.get("wait-ms").map(|v|v.parse::<u64>().map_err(|_|"invalid installer wait")).transpose()?.unwrap_or(0);
            crate::resume_wait(Path::new(need("transaction")?),Duration::from_millis(wait))},
        _=>unreachable!(),
    }
}
