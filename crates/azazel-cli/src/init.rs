//! `azazel init`: first-run setup of the default model and of a project's config file.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use azazel_ai::providers::http::{HttpProvider, http_client};
use azazel_ai::providers::presets::{self, ProviderConfig};
use azazel_ai::{Catalog, Provider};
use azazel_core::{Config, LoadOptions, Paths};
use toml_edit::{Array, DocumentMut, Item, Table};

/// How long a local server gets to answer before it counts as not running.
const LOCAL_PROBE: Duration = Duration::from_secs(2);

const NO_PROVIDER: &str = "no provider is ready. Set an API key in the environment (for example ANTHROPIC_API_KEY, \
                           OPENAI_API_KEY or GEMINI_API_KEY) or start a local model server, then run `azazel init` \
                           again";

fn read_document(config: &Path) -> Result<DocumentMut> {
    let text = match std::fs::read_to_string(config) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", config.display())),
    };
    text.parse()
        .with_context(|| format!("{} is not valid TOML", config.display()))
}

fn default_role_is_set(config: &Path) -> Result<bool> {
    Ok(read_document(config)?
        .get("roles")
        .and_then(|roles| roles.get("default"))
        .is_some())
}

/// Most links followed to find the file behind a symlinked config; more is a loop.
const MAX_LINKS: usize = 40;

/// The file `path` stands for when it is a symlink (a dotfile manager's config), followed to its
/// end; `path` itself when it is not one, or when the links cannot be followed.
fn resolve_link(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_LINKS {
        let is_link = std::fs::symlink_metadata(&current).is_ok_and(|meta| meta.file_type().is_symlink());
        let Some(target) = is_link.then(|| std::fs::read_link(&current).ok()).flatten() else {
            return current;
        };
        current = match current.parent() {
            Some(dir) => dir.join(target),
            None => target,
        };
    }
    path.to_path_buf()
}

/// Held while a config file is read, changed and replaced, so that two writers of one process
/// cannot each start from the text the other is about to replace.
static WRITING: Mutex<()> = Mutex::new(());

/// Makes the names of staged files differ between writers of one process.
static STAGED: AtomicU64 = AtomicU64::new(0);

/// Reads the config file at `config`, lets `change` edit it, and writes it back in one go, keeping
/// comments and everything else as written: the new text goes to a file beside it, with the
/// permissions of the old one, that is flushed to disk and then replaces it, so a failure leaves
/// the old file as it was. A symlinked `config` stays a link and its target is what is replaced.
/// Nothing is written when `change` fails. Writers of one process take turns; `change` must not
/// write a config file itself.
pub fn edit_config(config: &Path, change: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    edit_config_if(config, |doc| change(doc).map(|()| true))
}

/// [`edit_config`] for a `change` that says whether it changed anything: when it did not (`false`),
/// nothing is written, so the file is not created or rewritten for nothing.
fn edit_config_if(config: &Path, change: impl FnOnce(&mut DocumentMut) -> Result<bool>) -> Result<()> {
    let _turn = WRITING.lock().unwrap_or_else(PoisonError::into_inner);
    let config = resolve_link(config);
    let config = config.as_path();
    let mut doc = read_document(config)?;
    if !change(&mut doc)? {
        return Ok(());
    }
    if let Some(dir) = config.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let mut staged = config.as_os_str().to_owned();
    staged.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        STAGED.fetch_add(1, Ordering::Relaxed)
    ));
    let staged = PathBuf::from(staged);
    let stage = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&staged)?;
        // Before the text goes in, so that a private config is never readable by others.
        if let Ok(old) = std::fs::metadata(config) {
            file.set_permissions(old.permissions())?;
        }
        file.write_all(doc.to_string().as_bytes())?;
        file.sync_all()
    };
    if let Err(e) = stage() {
        let _ = std::fs::remove_file(&staged);
        return Err(e).with_context(|| format!("cannot write {}", staged.display()));
    }
    std::fs::rename(&staged, config).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        anyhow!("cannot write {}: {e}", config.display())
    })
}

/// Sets the key at `path` (`["editor", "mode"]`) in the config file at `config` to `value`, or
/// removes it when `value` is `None`, keeping comments and everything else as written. Missing
/// tables on the way are created for a value; a removal with a table missing has nothing to
/// remove and writes nothing.
pub fn set_config_value(config: &Path, path: &[&str], value: Option<Item>) -> Result<()> {
    let Some((key, tables)) = path.split_last() else {
        bail!("no setting named");
    };
    edit_config_if(config, |doc| {
        let mut table = doc.as_table_mut() as &mut dyn toml_edit::TableLike;
        for (depth, name) in tables.iter().enumerate() {
            if value.is_none() && !table.contains_key(name) {
                return Ok(false);
            }
            table = table
                .entry(name)
                .or_insert(Item::Table(Table::new()))
                .as_table_like_mut()
                .ok_or_else(|| {
                    anyhow!(
                        "`{}` in {} is not a table",
                        tables[..=depth].join("."),
                        config.display()
                    )
                })?;
        }
        match value {
            Some(value) => {
                table.insert(key, value);
            }
            None => {
                table.remove(key);
            }
        }
        Ok(true)
    })
}

/// Sets `roles.<role>` to `model` in the config file at `config`, or removes it when `model` is
/// `None`, leaving everything else as written.
pub fn write_role(config: &Path, role: &str, model: Option<&str>) -> Result<()> {
    let chain = model.map(|model| toml_edit::value(Array::from_iter([model])));
    set_config_value(config, &["roles", role], chain)
}

/// Lets the user pick one of `options` by number or by name. A single option is taken as is.
fn choose(what: &str, options: &[String], input: &mut impl BufRead) -> Result<String> {
    if let [only] = options {
        println!("{what}: {only}");
        return Ok(only.clone());
    }
    for (index, option) in options.iter().enumerate() {
        println!("{:>4}. {option}", index + 1);
    }
    loop {
        print!("{what} [1-{}]: ", options.len());
        std::io::stdout().flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            bail!("no choice was made");
        }
        let answer = line.trim();
        if let Ok(number) = answer.parse::<usize>()
            && let Some(option) = number.checked_sub(1).and_then(|index| options.get(index))
        {
            return Ok(option.clone());
        }
        if let Some(option) = options.iter().find(|option| *option == answer) {
            return Ok(option.clone());
        }
        println!("enter a number from the list");
    }
}

/// Model ids `provider` offers: what it lists, else what the built-in catalogue knows.
async fn model_ids(provider: &HttpProvider, id: &str) -> Vec<String> {
    let mut ids: Vec<String> = match provider.list_models().await {
        Ok(models) => models.into_iter().map(|model| model.id).collect(),
        Err(e) => {
            println!("could not list the models of {id} ({e}); using the built-in list");
            Vec::new()
        }
    };
    if ids.is_empty() {
        ids = Catalog::embedded()
            .for_provider(id)
            .into_iter()
            .map(|model| model.id.clone())
            .collect();
    }
    ids.sort();
    ids
}

/// Picks the default model and writes it to the home config, unless one is set already.
async fn global(paths: &Paths, input: &mut impl BufRead) -> Result<String> {
    let home_config = paths.home_config();
    if default_role_is_set(&home_config)? {
        return Ok(format!("{} already sets roles.default", home_config.display()));
    }
    let options = LoadOptions {
        trusted: false,
        overrides: toml::Table::new(),
        env: std::env::vars().collect(),
    };
    let config = Config::load(paths, &options)?.config;
    let credentials = Arc::new(crate::app::credentials(paths, false).configured(&config.providers));
    let client = http_client();
    let connect =
        |config: &ProviderConfig| HttpProvider::new(config.clone(), client.clone(), credentials.clone());

    // A provider is ready when it has a credential, or needs none and answers.
    let mut ready: Vec<(ProviderConfig, Option<Vec<String>>)> = Vec::new();
    for provider in presets::resolve(&config.providers).map_err(|e| anyhow!(e))? {
        if credentials.source(&provider).is_some() {
            ready.push((provider, None));
        } else if !provider.requires_key
            && let Ok(Ok(models)) = tokio::time::timeout(LOCAL_PROBE, connect(&provider).list_models()).await
            && !models.is_empty()
        {
            let mut ids: Vec<String> = models.into_iter().map(|model| model.id).collect();
            ids.sort();
            ready.push((provider, Some(ids)));
        }
    }
    if ready.is_empty() {
        bail!(NO_PROVIDER);
    }

    let names: Vec<String> = ready.iter().map(|(provider, _)| provider.id.clone()).collect();
    let chosen = choose("Provider", &names, input)?;
    let (provider, listed) = ready
        .into_iter()
        .find(|(provider, _)| provider.id == chosen)
        .ok_or_else(|| anyhow!("provider `{chosen}` is not in the list"))?;
    let models = match listed {
        Some(models) => models,
        None => model_ids(&connect(&provider), &provider.id).await,
    };
    if models.is_empty() {
        bail!("{chosen} offers no models");
    }
    let model = format!("{chosen}/{}", choose("Default model", &models, input)?);
    write_role(&home_config, "default", Some(&model))?;
    Ok(format!(
        "wrote roles.default = [\"{model}\"] to {}",
        home_config.display()
    ))
}

/// Writes `<project>/.azazel/config.toml` when the working directory is in a project that has none.
fn project(paths: &Paths) -> Result<Option<String>> {
    let config = paths.project_config();
    let in_project = paths.project_root.join(".git").exists() || paths.project_dir().is_dir();
    if !in_project || config == paths.home_config() || config.exists() {
        return Ok(None);
    }
    let dir = paths.project_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    // The commands the checker would detect, written out so that they can be read and changed.
    let mut commands = Array::new();
    for command in azazel_team::detect::verify_commands(&paths.project_root) {
        commands.push(command);
    }
    let mut verify = Table::new();
    verify.insert("commands", toml_edit::value(commands));
    let mut document = DocumentMut::new();
    document.insert("verify", Item::Table(verify));
    std::fs::write(&config, document.to_string())
        .with_context(|| format!("cannot write {}", config.display()))?;
    Ok(Some(format!("wrote {}", config.display())))
}

/// Runs `azazel init`. The exit code is 1 when a part failed.
pub async fn run(paths: &Paths) -> u8 {
    let mut failed = false;
    match global(paths, &mut std::io::stdin().lock()).await {
        Ok(done) => println!("{done}"),
        Err(e) => {
            eprintln!("azazel: {e:#}");
            failed = true;
        }
    }
    match project(paths) {
        Ok(Some(done)) => println!("{done}"),
        Ok(None) => {}
        Err(e) => {
            eprintln!("azazel: {e:#}");
            failed = true;
        }
    }
    u8::from(failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_a_key_below_a_missing_table_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        set_config_value(&config, &["tui", "theme"], None).unwrap();
        assert!(!config.exists(), "a removal created the file");
        std::fs::write(
            &config,
            "# mine
[editor]
mode = \"vim\"
",
        )
        .unwrap();
        set_config_value(&config, &["tui", "popups", "x"], None).unwrap();
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "# mine
[editor]
mode = \"vim\"
"
        );
    }

    #[test]
    fn a_nested_setting_is_written_kept_beside_comments_and_removed_again() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "# mine\n[editor]\nmode = \"vim\" # keep\n").unwrap();
        set_config_value(
            &config,
            &["editor", "fold_paste_lines"],
            Some(toml_edit::value(4)),
        )
        .unwrap();
        set_config_value(
            &config,
            &["tools", "format", "rs"],
            Some(toml_edit::value("rustfmt")),
        )
        .unwrap();
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(
            text.starts_with("# mine\n[editor]\nmode = \"vim\" # keep\nfold_paste_lines = 4\n"),
            "{text}"
        );
        let parsed: toml::Table = text.parse().unwrap();
        assert_eq!(parsed["tools"]["format"]["rs"].as_str(), Some("rustfmt"));

        set_config_value(&config, &["editor", "fold_paste_lines"], None).unwrap();
        assert!(
            !std::fs::read_to_string(&config)
                .unwrap()
                .contains("fold_paste_lines")
        );
        let error =
            set_config_value(&config, &["editor", "mode", "x"], Some(toml_edit::value(1))).unwrap_err();
        assert!(format!("{error:#}").contains("`editor.mode`"), "{error:#}");
    }

    /// Makes `link` a symlink to `target`; `false` where the platform or the user may not.
    fn symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(target, link);
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(target, link);
        made.is_ok()
    }

    #[test]
    fn a_symlinked_config_stays_a_link_and_its_target_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles").join("real.toml");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "# kept\n[tui]\ntheme = \"light\"\n").unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let config = home.join("config.toml");
        // A relative link, as dotfile managers make them, and a second hop through it.
        let hop = home.join("hop.toml");
        if !symlink(Path::new("..").join("dotfiles").join("real.toml").as_path(), &hop)
            || !symlink(Path::new("hop.toml"), &config)
        {
            // Creating links needs a privilege here; the resolution itself is covered below.
            assert_eq!(resolve_link(&config), config);
            return;
        }
        assert_eq!(
            azazel_core::paths::canonical(&resolve_link(&config)),
            azazel_core::paths::canonical(&real)
        );
        set_config_value(&config, &["editor", "mode"], Some(toml_edit::value("vim"))).unwrap();
        assert!(
            std::fs::symlink_metadata(&config)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the config is a regular file now"
        );
        assert!(std::fs::symlink_metadata(&hop).unwrap().file_type().is_symlink());
        let text = std::fs::read_to_string(&real).unwrap();
        assert_eq!(
            text,
            "# kept\n[tui]\ntheme = \"light\"\n\n[editor]\nmode = \"vim\"\n"
        );
    }

    #[test]
    fn a_path_that_is_not_a_link_resolves_to_itself() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("config.toml");
        assert_eq!(resolve_link(&plain), plain, "missing");
        std::fs::write(&plain, "").unwrap();
        assert_eq!(resolve_link(&plain), plain, "a file");
    }

    #[cfg(unix)]
    #[test]
    fn the_permissions_of_the_config_survive_a_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "[tui]\ntheme = \"light\"\n").unwrap();
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
        set_config_value(&config, &["tui", "theme"], Some(toml_edit::value("dark"))).unwrap();
        let mode = std::fs::metadata(&config).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(std::fs::read_to_string(&config).unwrap().contains("dark"));
    }

    #[test]
    fn writers_in_one_process_all_land_and_leave_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, "# mine\n").unwrap();
        let writers: Vec<_> = (0..16)
            .map(|n| {
                let config = config.clone();
                std::thread::spawn(move || {
                    for round in 0..5 {
                        let key = format!("k{n}_{round}");
                        set_config_value(&config, &["tools", "format", &key], Some(toml_edit::value(n)))
                            .unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let parsed: toml::Table = std::fs::read_to_string(&config).unwrap().parse().unwrap();
        assert_eq!(parsed["tools"]["format"].as_table().unwrap().len(), 80);
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 1, "{left:?}");
    }

    #[test]
    fn the_default_role_is_added_without_disturbing_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("home").join("config.toml");
        assert!(!default_role_is_set(&config).unwrap());
        write_role(&config, "default", Some("mock/a")).unwrap();
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "[roles]\ndefault = [\"mock/a\"]\n"
        );

        std::fs::write(
            &config,
            "# mine\n[roles]\nsmol = [\"mock/s\"] # small\n\n[tui]\ntheme = \"light\"\n",
        )
        .unwrap();
        write_role(&config, "default", Some("mock/b")).unwrap();
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "# mine\n[roles]\nsmol = [\"mock/s\"] # small\ndefault = [\"mock/b\"]\n\n[tui]\ntheme = \"light\"\n"
        );
        assert!(default_role_is_set(&config).unwrap());

        std::fs::write(&config, "roles = 3\n").unwrap();
        assert!(
            write_role(&config, "default", Some("mock/c"))
                .unwrap_err()
                .to_string()
                .contains("not a table")
        );
    }

    #[test]
    fn a_project_config_is_written_once_and_only_inside_a_project() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let loose = dir.path().join("loose");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&loose).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();

        let outside = Paths::discover(&loose, home.to_str());
        assert_eq!(project(&outside).unwrap(), None);
        assert!(!outside.project_config().exists());

        let inside = Paths::discover(&repo, home.to_str());
        assert!(project(&inside).unwrap().is_some());
        assert_eq!(
            std::fs::read_to_string(inside.project_config()).unwrap(),
            "[verify]\ncommands = []\n"
        );
        assert_eq!(project(&inside).unwrap(), None);
    }
}
