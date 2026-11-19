//! The command line of the `nisaba` binary.

use std::io::BufRead as _;
use std::path::PathBuf;

use crate::config::{PublicAddress, Settings};
use crate::store::Store;

/// The usage text.
#[must_use]
pub fn usage() -> String {
    format!(
        "usage: nisaba COMMAND [SETTINGS]

commands:
  serve                     run the server
  check                     check the settings and the data directory, and print them
  setup --email E [--name N]
                            make the first account, the administrator; reads the password
                            from the first line of standard input; works beside a running
                            server
  backup DEST               copy the data directory to DEST while the server runs; a second
                            run copies only what is new
  restore BACKUP            copy a backup into the data directory, which must be empty or new;
                            stop the server first
  packages add SPEC...      fill the package cache ahead of time, such as @preview/cetz:0.3.4
  api                       print the API description (api/openapi.json)
  help, version

settings (also from --config FILE or NISABA_CONFIG, and the environment):
  --config FILE                  a TOML file of settings
{}",
        crate::config::settings_help()
    )
}

/// Runs the command in `args`, the arguments after the program name, and
/// returns the exit status.
#[must_use]
pub fn main(args: &[String]) -> i32 {
    // The export child: no runtime, no logging, nothing but the compile.
    if args.first().map(String::as_str) == Some(crate::export::SUBCOMMAND) {
        return crate::export::child_main(&args[1..]);
    }
    let Some(command) = args.first().cloned() else {
        eprintln!("{}", usage());
        return 2;
    };
    let rest = &args[1..];
    let result = match command.as_str() {
        "help" | "--help" | "-h" => {
            println!("{}", usage());
            Ok(())
        }
        "version" | "--version" => {
            println!("nisaba {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "api" => {
            print!("{}", crate::api::description_text());
            Ok(())
        }
        "serve" => with_settings(rest, |settings, extra| {
            no_extra(&extra)?;
            runtime()?.block_on(crate::server::serve(settings))
        }),
        "check" => with_settings(rest, |settings, extra| {
            no_extra(&extra)?;
            check(&settings)
        }),
        "setup" => setup(rest),
        "backup" => with_settings(rest, |settings, extra| {
            let [dest] = extra.as_slice() else {
                return Err("backup needs one destination: nisaba backup DEST".into());
            };
            let report = crate::backup::backup(&settings.data, &PathBuf::from(dest))?;
            println!("backed up {} to {dest}: {report}", settings.data.display());
            Ok(())
        }),
        "restore" => with_settings(rest, |settings, extra| {
            let [from] = extra.as_slice() else {
                return Err("restore needs one backup: nisaba restore BACKUP".into());
            };
            let report = crate::backup::restore(&PathBuf::from(from), &settings.data)?;
            println!("restored {from} to {}: {report}", settings.data.display());
            Ok(())
        }),
        "packages" => with_settings(rest, |settings, extra| {
            let Some(("add", specs)) = extra.split_first().map(|(a, b)| (a.as_str(), b)) else {
                return Err("usage: nisaba packages add SPEC...".into());
            };
            if specs.is_empty() {
                return Err("name at least one package, such as @preview/cetz:0.3.4".into());
            }
            add_packages(&settings, specs)
        }),
        other => Err(format!("unknown command {other}\n\n{}", usage())),
    };
    match result {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("nisaba {command}: {message}");
            1
        }
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the runtime: {e}"))
}

fn with_settings(
    args: &[String],
    run: impl FnOnce(Settings, Vec<String>) -> Result<(), String>,
) -> Result<(), String> {
    let (settings, extra) = Settings::load(args, |name| std::env::var(name).ok())?;
    run(settings, extra)
}

fn no_extra(extra: &[String]) -> Result<(), String> {
    match extra.first() {
        Some(arg) => Err(format!("unexpected argument {arg}")),
        None => Ok(()),
    }
}

/// `nisaba check`: every setting, and whether the server could start with
/// them. Exits with 1 when something would stop it.
fn check(settings: &Settings) -> Result<(), String> {
    let mut problems = Vec::new();
    for (key, value) in settings.describe() {
        println!("{key} = {value}");
    }
    match settings.listen.parse() {
        Ok(listen) => match PublicAddress::resolve(
            listen,
            settings.public_url.as_deref(),
            settings.allow_insecure_http,
        ) {
            Ok(public) => println!("ok: the public address is {}", public.origin()),
            Err(e) => problems.push(e),
        },
        Err(_) => problems.push(format!("listen {} is not an address", settings.listen)),
    }
    let db = settings.data.join("nisaba.db");
    if db.is_file() {
        match Store::open_shared(&settings.data) {
            Ok(store) => match (store.user_count(), store.project_ids()) {
                (Ok(users), Ok(projects)) => println!(
                    "ok: the data directory holds {users} accounts and {} projects",
                    projects.len()
                ),
                (Err(e), _) | (_, Err(e)) => problems.push(e.to_string()),
            },
            Err(e) => problems.push(e.to_string()),
        }
    } else {
        println!(
            "ok: {} has no database yet; the server makes one at start",
            settings.data.display()
        );
    }
    match crate::fonts::Fonts::load(&settings.data.join("fonts")) {
        Ok(_) => println!("ok: the fonts in the data directory load"),
        Err(e) => problems.push(e),
    }
    let exporter = crate::export::Exporter::new(
        crate::export::Exporter::this_binary(),
        settings.export.clone(),
    );
    println!("{}", exporter.describe());
    if crate::web::built() {
        println!("ok: the web client is built in");
    } else {
        println!("note: this server was built without the web client; only the API works");
    }
    if problems.is_empty() {
        println!("the settings are fine");
        Ok(())
    } else {
        Err(problems.join("\n"))
    }
}

/// `nisaba setup --email E [--name N] [SETTINGS]`: the first account, from
/// the terminal of the server's machine. It needs no setup code, since only
/// someone who can run commands there can use it.
fn setup(args: &[String]) -> Result<(), String> {
    let mut email = None;
    let mut name = String::new();
    let mut rest = Vec::new();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--email" => email = Some(args.next().ok_or("--email needs a value")?.clone()),
            "--name" => name.clone_from(args.next().ok_or("--name needs a value")?),
            _ => rest.push(arg.clone()),
        }
    }
    let email = email.ok_or("setup needs --email")?;
    let (settings, extra) = Settings::load(&rest, |n| std::env::var(n).ok())?;
    no_extra(&extra)?;
    let email = email.trim().to_lowercase();
    if !email.contains('@') || email.len() > crate::limits::EMAIL_MAX {
        return Err(format!("{email} is not an email address"));
    }
    let mut password = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut password)
        .map_err(|e| format!("cannot read the password: {e}"))?;
    let password = password.trim_end_matches(['\r', '\n']).to_owned();
    if password.chars().count() < crate::limits::PASSWORD_MIN {
        return Err(format!(
            "the password needs at least {} characters",
            crate::limits::PASSWORD_MIN
        ));
    }
    let store = Store::open_shared(&settings.data).map_err(|e| e.to_string())?;
    if store.user_count().map_err(|e| e.to_string())? > 0 {
        return Err("this server already has an administrator".into());
    }
    let params = settings.passwords.params()?;
    let hash = crate::passwords::hash_with(&params, &password)?;
    let user = crate::api::User {
        id: crate::store::new_id(),
        name: if name.trim().is_empty() {
            email.clone()
        } else {
            name.trim().to_owned()
        },
        email,
        is_admin: true,
    };
    match store
        .create_owner(&user, &hash, None)
        .map_err(|e| e.to_string())?
    {
        crate::store::Setup::Created => {
            println!("made the administrator account {}", user.email);
            Ok(())
        }
        _ => Err("this server already has an administrator".into()),
    }
}

fn add_packages(settings: &Settings, specs: &[String]) -> Result<(), String> {
    let mut packages = settings.packages.clone();
    packages.download = true;
    let cache = crate::packages::Packages::new(settings.data.join("packages"), packages);
    let runtime = runtime()?;
    for text in specs {
        let spec = crate::packages::Spec::parse(text)
            .ok_or_else(|| format!("{text} is not a package such as @preview/cetz:0.3.4"))?;
        let bytes = runtime
            .block_on(cache.archive(&spec))
            .map_err(|e| match e {
                crate::packages::PackageError::NotFound(why)
                | crate::packages::PackageError::Unavailable(why) => why,
                crate::packages::PackageError::Store(e) => e.to_string(),
            })?;
        println!("{spec}: {} bytes in the cache", bytes.len());
    }
    Ok(())
}
