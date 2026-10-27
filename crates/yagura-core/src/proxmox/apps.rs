//! The app a container runs and whether it is the latest release. The
//! community-scripts installers leave the installed version in
//! `/root/.<app>` or `/opt/<app>_version.txt`; the latest one is GitHub's
//! `releases/latest`, fetched with `curl` (no HTTP client to carry) at most
//! once an hour per repository, because unauthenticated calls are limited to
//! 60 an hour.

use std::path::PathBuf;
use std::process::Command;

/// How long a fetched release is trusted.
const FRESH: u64 = 3600;

/// App name (lower case, no `-` or `_`) and its GitHub repository.
const REPOS: &[(&str, &str)] = &[
    ("immich", "immich-app/immich"),
    ("jellyfin", "jellyfin/jellyfin"),
    ("vaultwarden", "dani-garcia/vaultwarden"),
    ("paperless", "paperless-ngx/paperless-ngx"),
    ("paperlessngx", "paperless-ngx/paperless-ngx"),
    ("uptimekuma", "louislam/uptime-kuma"),
    ("grafana", "grafana/grafana"),
    ("prometheus", "prometheus/prometheus"),
    ("adguardhome", "AdguardTeam/AdGuardHome"),
    ("portainer", "portainer/portainer"),
    ("gitea", "go-gitea/gitea"),
    ("sonarr", "Sonarr/Sonarr"),
    ("radarr", "Radarr/Radarr"),
    ("lidarr", "Lidarr/Lidarr"),
    ("prowlarr", "Prowlarr/Prowlarr"),
    ("bazarr", "morpheus65535/bazarr"),
    ("overseerr", "sct/overseerr"),
    ("jellyseerr", "Fallenbagel/jellyseerr"),
    ("audiobookshelf", "advplyr/audiobookshelf"),
    ("homepage", "gethomepage/homepage"),
    ("memos", "usememos/memos"),
    ("n8n", "n8n-io/n8n"),
    ("mealie", "mealie-recipes/mealie"),
    ("navidrome", "navidrome/navidrome"),
    ("syncthing", "syncthing/syncthing"),
    ("tautulli", "Tautulli/Tautulli"),
    ("zigbee2mqtt", "Koenkk/zigbee2mqtt"),
    ("esphome", "esphome/esphome"),
    ("traefik", "traefik/traefik"),
    ("pocketbase", "pocketbase/pocketbase"),
    ("vikunja", "go-vikunja/vikunja"),
    ("wallos", "ellite/Wallos"),
    ("linkwarden", "linkwarden/linkwarden"),
    ("ollama", "ollama/ollama"),
    ("openwebui", "open-webui/open-webui"),
    ("couchdb", "apache/couchdb"),
    ("caddy", "caddyserver/caddy"),
    ("mosquitto", "eclipse-mosquitto/mosquitto"),
    ("influxdb", "influxdata/influxdb"),
];

/// An app's name as the table above spells it: lower case, letters and digits only.
pub fn key(app: &str) -> String {
    app.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

/// The repository of an app, by the name an installer used.
pub fn repo(app: &str) -> Option<&'static str> {
    let key = key(app);
    REPOS.iter().find(|(n, _)| *n == key).map(|(_, r)| *r)
}

/// Shell script that prints what a container runs: `path:first line` for the installer's files, then
/// `dpkg:<package>:<version>` for well-known services, `docker:<image>[:<tag>]` for running containers
/// and `couchdb:<version>` for a CouchDB unpacked under `/opt`.
pub const FIND: &str = r#"for f in /root/.[a-zA-Z]* /opt/*_version.txt; do [ -f "$f" ] && printf '%s:%s\n' "$f" "$(head -n 1 "$f" | head -c 64)"; done
for p in nginx apache2 postgresql mariadb-server mysql-server redis-server couchdb grafana prometheus caddy mosquitto influxdb docker-ce nodejs; do v=$(dpkg-query -W -f '${Version}' $p 2>/dev/null) && [ -n "$v" ] && printf 'dpkg:%s:%s\n' "$p" "$v"; done
command -v docker >/dev/null 2>&1 && docker ps --format '{{.Image}}' 2>/dev/null | while read -r i; do printf 'docker:%s\n' "$i"; done
[ -f /opt/couchdb/releases/start_erl.data ] && printf 'couchdb:%s\n' "$(cut -d' ' -f2 /opt/couchdb/releases/start_erl.data)"
true"#;

/// A Debian version as the upstream one: no epoch (`1:`) and no packaging revision (`-1~bookworm`).
fn upstream(v: &str) -> &str {
    let v = v.split_once(':').map_or(v, |(_, r)| r);
    v.split(['-', '+', '~']).next().unwrap_or(v)
}

/// `(app, version)` from the output of [`FIND`]; a file whose first line is
/// not a version (`.bashrc`, `.profile`) is not an app, nor is an image without a numeric tag.
pub fn parse_found(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut add = |name: &str, ver: &str| {
        let ver = ver.trim();
        if numbers(ver).is_some() && !out.iter().any(|(n, _)| key(n) == key(name)) {
            out.push((name.to_string(), ver.to_string()));
        }
    };
    for l in text.lines() {
        if let Some(r) = l.strip_prefix("dpkg:") {
            if let Some((name, ver)) = r.split_once(':') {
                add(name, upstream(ver));
            }
        } else if let Some(r) = l.strip_prefix("docker:") {
            // `ghcr.io/immich-app/immich-server:v1.2` is `immich-server` at `v1.2`.
            if let Some((image, tag)) = r.rsplit_once(':').filter(|(_, t)| !t.contains('/')) {
                add(image.rsplit('/').next().unwrap_or(image), tag);
            }
        } else if let Some(ver) = l.strip_prefix("couchdb:") {
            add("couchdb", ver);
        } else if let Some((path, ver)) = l.split_once(':') {
            let file = path.rsplit('/').next().unwrap_or("");
            if let Some(name) = file.strip_prefix('.').or_else(|| file.strip_suffix("_version.txt")) {
                add(name, ver);
            }
        }
    }
    out
}

/// The dotted numbers of a version (`v3.3.5` is `[3, 3, 5]`); `None` if it
/// does not start like one.
fn numbers(v: &str) -> Option<Vec<u64>> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let end = v.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(v.len());
    let parts: Vec<u64> = v[..end].split('.').filter(|p| !p.is_empty()).map(|p| p.parse().ok()).collect::<Option<_>>()?;
    (!parts.is_empty()).then_some(parts)
}

/// Whether `installed` is behind `latest`. Versions that do not read as
/// numbers are never called old: better silent than wrong.
pub fn is_older(installed: &str, latest: &str) -> bool {
    match (numbers(installed), numbers(latest)) {
        (Some(mut a), Some(mut b)) => {
            let n = a.len().max(b.len());
            a.resize(n, 0);
            b.resize(n, 0);
            a < b
        }
        _ => false,
    }
}

/// `tag_name` of GitHub's `releases/latest` answer.
pub fn parse_tag(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get("tag_name")?.as_str().map(str::to_string)
}

fn cache_file(repo: &str) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("yagura").join(format!("release-{}", repo.replace('/', "-"))))
}

/// `epoch\ntag` as the cache holds it: the tag, and whether it is still fresh.
fn parse_cache(text: &str, now: u64) -> Option<(String, bool)> {
    let (at, tag) = text.split_once('\n')?;
    let at: u64 = at.trim().parse().ok()?;
    let tag = tag.trim();
    (!tag.is_empty()).then(|| (tag.to_string(), now.saturating_sub(at) < FRESH))
}

/// The latest release of `app`: from the cache while it is fresh, otherwise
/// from GitHub (the stale cache stands in if that fails). Slow: call it from a
/// thread of its own.
pub fn latest(app: &str) -> Option<String> {
    let repo = repo(app)?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let file = cache_file(repo);
    let cached = file.as_ref().and_then(|f| std::fs::read_to_string(f).ok()).and_then(|t| parse_cache(&t, now));
    if let Some((tag, true)) = &cached {
        return Some(tag.clone());
    }
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let mut cmd = Command::new("curl");
    cmd.args(["-fsS", "-m", "10", "-H", "User-Agent: yagura", "-H", "Accept: application/vnd.github+json"]);
    if let Ok(t) = std::env::var("GITHUB_TOKEN") {
        cmd.args(["-H", &format!("Authorization: Bearer {t}")]);
    }
    let fetched = cmd.arg(url).output().ok().filter(|o| o.status.success()).and_then(|o| parse_tag(&String::from_utf8_lossy(&o.stdout)));
    match fetched {
        Some(tag) => {
            if let Some(f) = file {
                let _ = f.parent().map(std::fs::create_dir_all);
                let _ = std::fs::write(f, format!("{now}\n{tag}\n"));
            }
            Some(tag)
        }
        None => cached.map(|(tag, _)| tag),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_and_docker_images_become_apps() {
        let out = "dpkg:nginx:1.22.1-9+deb12u1\ndpkg:redis-server:5:7.0.15-1~deb12u2\ndocker:ghcr.io/immich-app/immich-server:v1.2.3\ndocker:redis:latest\ndocker:localhost:5000/web:2.0\ncouchdb:3.5.0\n";
        assert_eq!(
            parse_found(out),
            [("nginx", "1.22.1"), ("redis-server", "7.0.15"), ("immich-server", "v1.2.3"), ("web", "2.0"), ("couchdb", "3.5.0")]
                .map(|(a, b)| (a.to_string(), b.to_string()))
        );
    }

    #[test]
    fn installer_files_become_apps_and_dotfiles_do_not() {
        let out = "/root/.bashrc:# ~/.bashrc: executed by bash\n/root/.immich:3.1.0\n/opt/vaultwarden_version.txt:v1.32.0\n/root/.profile:if [ -n x ]\n";
        assert_eq!(parse_found(out), [("immich".to_string(), "3.1.0".to_string()), ("vaultwarden".to_string(), "v1.32.0".to_string())]);
    }

    #[test]
    fn versions_compare_by_their_numbers() {
        assert!(is_older("3.1.0", "v3.3.5"));
        assert!(is_older("v1.9", "1.10.0"));
        assert!(!is_older("3.3.5", "v3.3.5"));
        assert!(!is_older("3.3.5.1", "3.3.5"));
        assert!(!is_older("nightly", "3.3.5"));
    }

    #[test]
    fn apps_are_found_by_name_whatever_the_dashes() {
        assert_eq!(repo("uptime-kuma"), Some("louislam/uptime-kuma"));
        assert_eq!(repo("Immich"), Some("immich-app/immich"));
        assert_eq!(repo("unknown"), None);
    }

    #[test]
    fn github_answers_and_the_cache_are_read() {
        assert_eq!(parse_tag(r#"{"tag_name":"v3.3.5","draft":false}"#).as_deref(), Some("v3.3.5"));
        assert_eq!(parse_tag("{}"), None);
        assert_eq!(parse_cache("1000\nv1\n", 1500), Some(("v1".into(), true)));
        assert_eq!(parse_cache("1000\nv1\n", 1000 + FRESH), Some(("v1".into(), false)));
        assert_eq!(parse_cache("junk", 0), None);
    }
}
