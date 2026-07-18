//! Interpreter: command tree -> a `Spec`. Recognizes the widget verbs,
//! `source .x { … }` whose body compiles to a query pipeline (see `query`), and
//! the `.x <- in` bind shorthand. Unknown widget verbs are ignored so specs stay
//! forward-compatible.

use std::collections::BTreeMap;

use regex::Regex;

use crate::ast::{Arg, Command};
use crate::query::{FieldSel, QueryOp};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WidgetKind {
    Text,
    Tail,
    List,
    Gauge,
    Bars,
    Histo,
    Spark,
    Chart,
    Table,
    Tabs,
    Block,
    Frame,
}

impl WidgetKind {
    fn from(verb: &str) -> Option<WidgetKind> {
        Some(match verb {
            "text" => WidgetKind::Text,
            "tail" => WidgetKind::Tail,
            "list" => WidgetKind::List,
            "gauge" => WidgetKind::Gauge,
            "bars" => WidgetKind::Bars,
            "histo" => WidgetKind::Histo,
            "spark" => WidgetKind::Spark,
            "chart" => WidgetKind::Chart,
            "table" => WidgetKind::Table,
            "tabs" => WidgetKind::Tabs,
            "block" => WidgetKind::Block,
            "frame" => WidgetKind::Frame,
            _ => return None,
        })
    }

    pub fn label(&self) -> &'static str {
        match self {
            WidgetKind::Text => "text",
            WidgetKind::Tail => "tail",
            WidgetKind::List => "list",
            WidgetKind::Gauge => "gauge",
            WidgetKind::Bars => "bars",
            WidgetKind::Histo => "histo",
            WidgetKind::Spark => "spark",
            WidgetKind::Chart => "chart",
            WidgetKind::Table => "table",
            WidgetKind::Tabs => "tabs",
            WidgetKind::Block => "block",
            WidgetKind::Frame => "frame",
        }
    }
}

/// A data source: reads stdin, then applies a query pipeline.
#[derive(Debug, Clone)]
pub struct Source {
    pub pipeline: Vec<QueryOp>,
}

#[derive(Debug, Clone)]
pub struct Widget {
    pub path: String,
    pub kind: WidgetKind,
    pub opts: BTreeMap<String, String>,
    pub source: Option<Source>,
    /// Grid cell `(row, col)` set by a `grid` command; `None` = auto-stacked.
    pub grid: Option<(usize, usize)>,
}

#[derive(Debug, Default)]
pub struct Spec {
    pub widgets: Vec<Widget>,
    /// Downstream output pipeline (`out { … }`): applied to the stream and
    /// written to stdout, so arb can *modify* a pipe, not just visualize it.
    pub out: Option<Vec<QueryOp>>,
}

/// Build a `Spec` from a parsed command tree.
pub fn build(cmds: &[Command]) -> Result<Spec, String> {
    let mut spec = Spec::default();
    build_into(&mut spec, cmds, 0)?;
    Ok(spec)
}

/// Process `cmds` into `spec`. `import NAME` resolves and inlines a module
/// (stdlib preset or user file); `depth` guards against import cycles.
fn build_into(spec: &mut Spec, cmds: &[Command], depth: usize) -> Result<(), String> {
    if depth > 16 {
        return Err("import: module nesting too deep (cycle?)".into());
    }
    for c in cmds {
        if c.name == "import" {
            let name = c
                .args
                .first()
                .and_then(Arg::as_str)
                .ok_or("import: missing module name")?;
            let src = resolve_module(name)?;
            let sub = crate::parser::parse(&src)?;
            build_into(spec, &sub, depth + 1)?;
        } else if let Some(kind) = WidgetKind::from(&c.name) {
            let path = c
                .args
                .first()
                .and_then(Arg::as_str)
                .ok_or_else(|| format!("{}: missing widget path", c.name))?;
            if !path.starts_with('.') {
                return Err(format!(
                    "{}: widget path must start with '.', got `{path}`",
                    c.name
                ));
            }
            spec.widgets.push(Widget {
                path: path.to_string(),
                kind,
                opts: parse_opts(&c.args[1..]),
                source: None,
                grid: None,
            });
        } else if c.name == "source" {
            let path = c
                .args
                .first()
                .and_then(Arg::as_str)
                .ok_or("source: missing path")?;
            let body = match c.args.get(1) {
                Some(Arg::Block(b)) => b,
                _ => return Err("source: expected `{ body }`".into()),
            };
            let pipeline = pipeline_from_body(body)?;
            set_source(spec, path, Source { pipeline })?;
        } else if c.name == "out" {
            let body = match c.args.first() {
                Some(Arg::Block(b)) => b,
                _ => return Err("out: expected `{ body }`".into()),
            };
            spec.out = Some(pipeline_from_body(body)?);
        } else if c.name == "grid" {
            let path = c
                .args
                .first()
                .and_then(Arg::as_str)
                .ok_or("grid: missing path")?;
            let o = parse_opts(&c.args[1..]);
            let cell = |k| {
                o.get(k)
                    .and_then(|s: &String| s.parse::<usize>().ok())
                    .unwrap_or(0)
            };
            set_grid(spec, path, (cell("row"), cell("col")))?;
        } else if c.name.starts_with('.') {
            // `.path <- in` bind shorthand (empty pipeline). `configure` etc. later.
            if c.args.first().and_then(Arg::as_str) == Some("<-")
                && c.args.get(1).and_then(Arg::as_str) == Some("in")
            {
                let path = c.name.clone();
                set_source(spec, &path, Source { pipeline: vec![] })?;
            }
        }
        // Unknown verbs are ignored.
    }
    Ok(())
}

/// Resolve a module name to its source: a local `NAME.arb`, then
/// `~/.arb/lib/NAME.arb`, then a bundled stdlib preset.
fn resolve_module(name: &str) -> Result<String, String> {
    let local = format!("{name}.arb");
    if let Ok(s) = std::fs::read_to_string(&local) {
        return Ok(s);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = std::path::Path::new(&home)
            .join(".arb/lib")
            .join(format!("{name}.arb"));
        if let Ok(s) = std::fs::read_to_string(p) {
            return Ok(s);
        }
    }
    bundled_module(name)
        .map(str::to_string)
        .ok_or_else(|| format!("import: module `{name}` not found"))
}

/// Names of the bundled stdlib presets.
pub const STDLIB_NAMES: &[&str] = &[
    "nums", "logs", "http", "json", "table", "top", "docker", "k8s", "nginx", "git", "systemd",
    "redis", "postgres", "mysql", "mongodb", "kafka", "prometheus", "elasticsearch", "rabbitmq",
    "apache", "haproxy", "journalctl", "dmesg", "ps", "htop", "iostat", "vmstat", "ss", "dig",
    "curl", "gh", "terraform", "aws", "gcloud", "azure", "ansible", "consul", "vault", "etcd",
    "nomad", "envoy", "memcached", "varnish", "pgbouncer", "celery", "sidekiq", "gunicorn",
    "supervisor", "fail2ban", "iptables", "conntrack", "sar",
    "nats",
    "tomcat",
    "puma",
    "lighttpd",
    "helm",
    "podman",
    "containerd",
    "crictl",
    "istioctl",
    "linkerd",
    "cilium",
    "argocd",
    "fluxcd",
    "velero",
    "kustomize",
    "skopeo",
    "buildah",
    "stern",
    "kubectx",
    "jenkins",
    "gitlabrunner",
    "circleci",
    "drone",
    "buildkite",
    "concourse",
    "woodpecker",
    "spinnaker",
    "tekton",
    "teamcity",
    "packer",
    "vagrant",
    "pulumi",
    "chef",
    "puppet",
    "salt",
    "cloudformation",
    "cdk",
    "doctl",
    "hcloud",
    "fly",
    "heroku",
    "linode",
    "vultr",
    "scaleway",
    "cassandra",
    "scylla",
    "cockroachdb",
    "clickhouse",
    "influxdb",
    "neo4j",
    "couchdb",
    "dynamodb",
    "mariadb",
    "duckdb",
    "sqlite",
    "mssql",
    "timescaledb",
    "arangodb",
    "tidb",
    "victoriametrics",
    "questdb",
    "riak",
    "dgraph",
    "opensearch",
    "grafana",
    "loki",
    "tempo",
    "jaeger",
    "zipkin",
    "datadog",
    "newrelic",
    "sentry",
    "statsd",
    "telegraf",
    "collectd",
    "netdata",
    "zabbix",
    "nagios",
    "icinga",
    "pulsar",
    "activemq",
    "nsq",
    "beanstalkd",
    "caddy",
    "traefik",
    "uwsgi",
    "phpfpm",
    "tcpdump",
    "tshark",
    "nmap",
    "netstat",
    "iftop",
    "nethogs",
    "vnstat",
    "mtr",
    "zfs",
    "btrfs",
    "smartctl",
    "nvme",
    "rclone",
];

fn bundled_module(name: &str) -> Option<&'static str> {
    Some(match name {
        "nums" => include_str!("../stdlib/nums.arb"),
        "logs" => include_str!("../stdlib/logs.arb"),
        "http" => include_str!("../stdlib/http.arb"),
        "json" => include_str!("../stdlib/json.arb"),
        "table" => include_str!("../stdlib/table.arb"),
        "top" => include_str!("../stdlib/top.arb"),
        "docker" => include_str!("../stdlib/docker.arb"),
        "k8s" => include_str!("../stdlib/k8s.arb"),
        "nginx" => include_str!("../stdlib/nginx.arb"),
        "git" => include_str!("../stdlib/git.arb"),
        "systemd" => include_str!("../stdlib/systemd.arb"),
        "redis" => include_str!("../stdlib/redis.arb"),
        "postgres" => include_str!("../stdlib/postgres.arb"),
        "mysql" => include_str!("../stdlib/mysql.arb"),
        "mongodb" => include_str!("../stdlib/mongodb.arb"),
        "kafka" => include_str!("../stdlib/kafka.arb"),
        "prometheus" => include_str!("../stdlib/prometheus.arb"),
        "elasticsearch" => include_str!("../stdlib/elasticsearch.arb"),
        "rabbitmq" => include_str!("../stdlib/rabbitmq.arb"),
        "apache" => include_str!("../stdlib/apache.arb"),
        "haproxy" => include_str!("../stdlib/haproxy.arb"),
        "journalctl" => include_str!("../stdlib/journalctl.arb"),
        "dmesg" => include_str!("../stdlib/dmesg.arb"),
        "ps" => include_str!("../stdlib/ps.arb"),
        "htop" => include_str!("../stdlib/htop.arb"),
        "iostat" => include_str!("../stdlib/iostat.arb"),
        "vmstat" => include_str!("../stdlib/vmstat.arb"),
        "ss" => include_str!("../stdlib/ss.arb"),
        "dig" => include_str!("../stdlib/dig.arb"),
        "curl" => include_str!("../stdlib/curl.arb"),
        "gh" => include_str!("../stdlib/gh.arb"),
        "terraform" => include_str!("../stdlib/terraform.arb"),
        "aws" => include_str!("../stdlib/aws.arb"),
        "gcloud" => include_str!("../stdlib/gcloud.arb"),
        "azure" => include_str!("../stdlib/azure.arb"),
        "ansible" => include_str!("../stdlib/ansible.arb"),
        "consul" => include_str!("../stdlib/consul.arb"),
        "vault" => include_str!("../stdlib/vault.arb"),
        "etcd" => include_str!("../stdlib/etcd.arb"),
        "nomad" => include_str!("../stdlib/nomad.arb"),
        "envoy" => include_str!("../stdlib/envoy.arb"),
        "memcached" => include_str!("../stdlib/memcached.arb"),
        "varnish" => include_str!("../stdlib/varnish.arb"),
        "pgbouncer" => include_str!("../stdlib/pgbouncer.arb"),
        "celery" => include_str!("../stdlib/celery.arb"),
        "sidekiq" => include_str!("../stdlib/sidekiq.arb"),
        "gunicorn" => include_str!("../stdlib/gunicorn.arb"),
        "supervisor" => include_str!("../stdlib/supervisor.arb"),
        "fail2ban" => include_str!("../stdlib/fail2ban.arb"),
        "iptables" => include_str!("../stdlib/iptables.arb"),
        "conntrack" => include_str!("../stdlib/conntrack.arb"),
        "sar" => include_str!("../stdlib/sar.arb"),
        "nats" => include_str!("../stdlib/nats.arb"),
        "tomcat" => include_str!("../stdlib/tomcat.arb"),
        "puma" => include_str!("../stdlib/puma.arb"),
        "lighttpd" => include_str!("../stdlib/lighttpd.arb"),
        "helm" => include_str!("../stdlib/helm.arb"),
        "podman" => include_str!("../stdlib/podman.arb"),
        "containerd" => include_str!("../stdlib/containerd.arb"),
        "crictl" => include_str!("../stdlib/crictl.arb"),
        "istioctl" => include_str!("../stdlib/istioctl.arb"),
        "linkerd" => include_str!("../stdlib/linkerd.arb"),
        "cilium" => include_str!("../stdlib/cilium.arb"),
        "argocd" => include_str!("../stdlib/argocd.arb"),
        "fluxcd" => include_str!("../stdlib/fluxcd.arb"),
        "velero" => include_str!("../stdlib/velero.arb"),
        "kustomize" => include_str!("../stdlib/kustomize.arb"),
        "skopeo" => include_str!("../stdlib/skopeo.arb"),
        "buildah" => include_str!("../stdlib/buildah.arb"),
        "stern" => include_str!("../stdlib/stern.arb"),
        "kubectx" => include_str!("../stdlib/kubectx.arb"),
        "jenkins" => include_str!("../stdlib/jenkins.arb"),
        "gitlabrunner" => include_str!("../stdlib/gitlabrunner.arb"),
        "circleci" => include_str!("../stdlib/circleci.arb"),
        "drone" => include_str!("../stdlib/drone.arb"),
        "buildkite" => include_str!("../stdlib/buildkite.arb"),
        "concourse" => include_str!("../stdlib/concourse.arb"),
        "woodpecker" => include_str!("../stdlib/woodpecker.arb"),
        "spinnaker" => include_str!("../stdlib/spinnaker.arb"),
        "tekton" => include_str!("../stdlib/tekton.arb"),
        "teamcity" => include_str!("../stdlib/teamcity.arb"),
        "packer" => include_str!("../stdlib/packer.arb"),
        "vagrant" => include_str!("../stdlib/vagrant.arb"),
        "pulumi" => include_str!("../stdlib/pulumi.arb"),
        "chef" => include_str!("../stdlib/chef.arb"),
        "puppet" => include_str!("../stdlib/puppet.arb"),
        "salt" => include_str!("../stdlib/salt.arb"),
        "cloudformation" => include_str!("../stdlib/cloudformation.arb"),
        "cdk" => include_str!("../stdlib/cdk.arb"),
        "doctl" => include_str!("../stdlib/doctl.arb"),
        "hcloud" => include_str!("../stdlib/hcloud.arb"),
        "fly" => include_str!("../stdlib/fly.arb"),
        "heroku" => include_str!("../stdlib/heroku.arb"),
        "linode" => include_str!("../stdlib/linode.arb"),
        "vultr" => include_str!("../stdlib/vultr.arb"),
        "scaleway" => include_str!("../stdlib/scaleway.arb"),
        "cassandra" => include_str!("../stdlib/cassandra.arb"),
        "scylla" => include_str!("../stdlib/scylla.arb"),
        "cockroachdb" => include_str!("../stdlib/cockroachdb.arb"),
        "clickhouse" => include_str!("../stdlib/clickhouse.arb"),
        "influxdb" => include_str!("../stdlib/influxdb.arb"),
        "neo4j" => include_str!("../stdlib/neo4j.arb"),
        "couchdb" => include_str!("../stdlib/couchdb.arb"),
        "dynamodb" => include_str!("../stdlib/dynamodb.arb"),
        "mariadb" => include_str!("../stdlib/mariadb.arb"),
        "duckdb" => include_str!("../stdlib/duckdb.arb"),
        "sqlite" => include_str!("../stdlib/sqlite.arb"),
        "mssql" => include_str!("../stdlib/mssql.arb"),
        "timescaledb" => include_str!("../stdlib/timescaledb.arb"),
        "arangodb" => include_str!("../stdlib/arangodb.arb"),
        "tidb" => include_str!("../stdlib/tidb.arb"),
        "victoriametrics" => include_str!("../stdlib/victoriametrics.arb"),
        "questdb" => include_str!("../stdlib/questdb.arb"),
        "riak" => include_str!("../stdlib/riak.arb"),
        "dgraph" => include_str!("../stdlib/dgraph.arb"),
        "opensearch" => include_str!("../stdlib/opensearch.arb"),
        "grafana" => include_str!("../stdlib/grafana.arb"),
        "loki" => include_str!("../stdlib/loki.arb"),
        "tempo" => include_str!("../stdlib/tempo.arb"),
        "jaeger" => include_str!("../stdlib/jaeger.arb"),
        "zipkin" => include_str!("../stdlib/zipkin.arb"),
        "datadog" => include_str!("../stdlib/datadog.arb"),
        "newrelic" => include_str!("../stdlib/newrelic.arb"),
        "sentry" => include_str!("../stdlib/sentry.arb"),
        "statsd" => include_str!("../stdlib/statsd.arb"),
        "telegraf" => include_str!("../stdlib/telegraf.arb"),
        "collectd" => include_str!("../stdlib/collectd.arb"),
        "netdata" => include_str!("../stdlib/netdata.arb"),
        "zabbix" => include_str!("../stdlib/zabbix.arb"),
        "nagios" => include_str!("../stdlib/nagios.arb"),
        "icinga" => include_str!("../stdlib/icinga.arb"),
        "pulsar" => include_str!("../stdlib/pulsar.arb"),
        "activemq" => include_str!("../stdlib/activemq.arb"),
        "nsq" => include_str!("../stdlib/nsq.arb"),
        "beanstalkd" => include_str!("../stdlib/beanstalkd.arb"),
        "caddy" => include_str!("../stdlib/caddy.arb"),
        "traefik" => include_str!("../stdlib/traefik.arb"),
        "uwsgi" => include_str!("../stdlib/uwsgi.arb"),
        "phpfpm" => include_str!("../stdlib/phpfpm.arb"),
        "tcpdump" => include_str!("../stdlib/tcpdump.arb"),
        "tshark" => include_str!("../stdlib/tshark.arb"),
        "nmap" => include_str!("../stdlib/nmap.arb"),
        "netstat" => include_str!("../stdlib/netstat.arb"),
        "iftop" => include_str!("../stdlib/iftop.arb"),
        "nethogs" => include_str!("../stdlib/nethogs.arb"),
        "vnstat" => include_str!("../stdlib/vnstat.arb"),
        "mtr" => include_str!("../stdlib/mtr.arb"),
        "zfs" => include_str!("../stdlib/zfs.arb"),
        "btrfs" => include_str!("../stdlib/btrfs.arb"),
        "smartctl" => include_str!("../stdlib/smartctl.arb"),
        "nvme" => include_str!("../stdlib/nvme.arb"),
        "rclone" => include_str!("../stdlib/rclone.arb"),
        _ => return None,
    })
}

/// List available presets as `(name, description)` — bundled stdlib plus any
/// user modules in `~/.arb/lib`. The description is the preset's first `#` line.
pub fn list_presets() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = STDLIB_NAMES
        .iter()
        .map(|n| {
            (
                n.to_string(),
                first_comment(bundled_module(n).unwrap_or("")),
            )
        })
        .collect();
    if let Some(home) = std::env::var_os("HOME") {
        let dir = std::path::Path::new(&home).join(".arb/lib");
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().and_then(|s| s.to_str()) == Some("arb") {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        let src = std::fs::read_to_string(&path).unwrap_or_default();
                        out.push((stem.to_string(), first_comment(&src)));
                    }
                }
            }
        }
    }
    out
}

fn first_comment(src: &str) -> String {
    src.lines()
        .find_map(|l| l.trim().strip_prefix('#'))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Collect `-flag value` pairs into an options map.
fn parse_opts(args: &[Arg]) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(flag) = args[i].as_str().and_then(|w| w.strip_prefix('-')) {
            let val = args
                .get(i + 1)
                .and_then(Arg::as_str)
                .unwrap_or("")
                .to_string();
            m.insert(flag.to_string(), val);
            i += 2;
        } else {
            i += 1;
        }
    }
    m
}

/// Compile a `source { … }` body into a query pipeline. Must start with `in`.
fn pipeline_from_body(cmds: &[Command]) -> Result<Vec<QueryOp>, String> {
    let mut ops = Vec::new();
    let mut saw_in = false;
    for c in cmds {
        match c.name.as_str() {
            "in" | "in.json" | "in.html" | "in.xml" | "in.logfmt" => saw_in = true,
            "in.csv" => {
                saw_in = true;
                ops.push(QueryOp::Csv);
            }
            "in.tsv" => {
                saw_in = true;
                ops.push(QueryOp::Tsv);
            }
            "in.yaml" | "in.yml" => {
                saw_in = true;
                ops.push(QueryOp::Yaml);
            }
            "in.toml" => {
                saw_in = true;
                ops.push(QueryOp::Toml);
            }
            "sel" => {
                let words: Vec<&str> = c.args.iter().filter_map(Arg::as_str).collect();
                let mut css_parts = Vec::new();
                let mut attr = None;
                let mut i = 0;
                while i < words.len() {
                    if words[i] == "-attr" {
                        attr = words.get(i + 1).map(|s| s.to_string());
                        i += 2;
                    } else {
                        css_parts.push(words[i]);
                        i += 1;
                    }
                }
                let css = css_parts.join(" ");
                if css.trim().is_empty() {
                    return Err("sel: expected a CSS selector".into());
                }
                ops.push(QueryOp::Sel { css, attr });
            }
            "find" => {
                let css = c
                    .args
                    .iter()
                    .filter_map(Arg::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                if css.trim().is_empty() {
                    return Err("find: expected a tag/selector".into());
                }
                ops.push(QueryOp::Find(css));
            }
            "attr" => {
                let name = str_arg(c);
                if name.is_empty() {
                    return Err("attr: expected an attribute name".into());
                }
                ops.push(QueryOp::Attr(name));
            }
            "text" => ops.push(QueryOp::Text),
            "match" | "grep" => ops.push(QueryOp::Match(regex_arg(c)?)),
            "reject" | "grepv" => ops.push(QueryOp::Reject(regex_arg(c)?)),
            "field" => ops.push(QueryOp::Field(field_sel(&c.args)?)),
            "each" => ops.push(QueryOp::Each),
            "count" => ops.push(QueryOp::Count),
            "rate" => ops.push(QueryOp::Rate),
            "tally" => ops.push(QueryOp::Tally),
            "sum" => ops.push(QueryOp::Sum),
            "min" => ops.push(QueryOp::Min),
            "max" => ops.push(QueryOp::Max),
            "avg" => ops.push(QueryOp::Avg),
            "keys" => ops.push(QueryOp::Keys),
            "vals" => ops.push(QueryOp::Vals),
            "pick" => {
                let keys: Vec<String> = c
                    .args
                    .iter()
                    .filter_map(Arg::as_str)
                    .map(str::to_string)
                    .collect();
                if keys.is_empty() {
                    return Err("pick: expected one or more key names".into());
                }
                ops.push(QueryOp::Pick(keys));
            }
            "sort" => {
                let flags: Vec<&str> = c.args.iter().filter_map(Arg::as_str).collect();
                ops.push(QueryOp::Sort {
                    numeric: flags.contains(&"-n"),
                    reverse: flags.contains(&"-r"),
                });
            }
            "uniq" => ops.push(QueryOp::Uniq),
            "rev" => ops.push(QueryOp::Rev),
            "first" => ops.push(QueryOp::First),
            "last" => ops.push(QueryOp::Last),
            "upper" => ops.push(QueryOp::Upper),
            "lower" => ops.push(QueryOp::Lower),
            "trim" => ops.push(QueryOp::Trim),
            "replace" => {
                let re = regex_arg(c)?;
                let to = c
                    .args
                    .get(1)
                    .and_then(Arg::as_str)
                    .unwrap_or("")
                    .to_string();
                ops.push(QueryOp::Replace(re, to));
            }
            "join" => {
                let sep = c
                    .args
                    .first()
                    .and_then(Arg::as_str)
                    .unwrap_or(" ")
                    .to_string();
                ops.push(QueryOp::Join(sep));
            }
            "nth" => ops.push(QueryOp::Nth(count_arg(c, "nth")?)),
            "take" => ops.push(QueryOp::Take(count_arg(c, "take")?)),
            "drop" => ops.push(QueryOp::Drop(count_arg(c, "drop")?)),
            "calc" => {
                let src = c
                    .args
                    .iter()
                    .filter_map(Arg::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                ops.push(QueryOp::Calc(crate::expr::parse(&src)?));
            }
            "where" => {
                let src = c
                    .args
                    .iter()
                    .filter_map(Arg::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                ops.push(QueryOp::Where(crate::expr::parse(&src)?));
            }
            "map" => {
                let src = c
                    .args
                    .iter()
                    .filter_map(Arg::as_str)
                    .collect::<Vec<_>>()
                    .join(" ");
                ops.push(QueryOp::Map(crate::expr::parse(&src)?));
            }
            "sort_by" => {
                let field = str_arg(c);
                if field.is_empty() {
                    return Err("sort_by: expected a field name".into());
                }
                ops.push(QueryOp::SortBy(field));
            }
            "unique_by" => {
                let field = str_arg(c);
                if field.is_empty() {
                    return Err("unique_by: expected a field name".into());
                }
                ops.push(QueryOp::UniqueBy(field));
            }
            "count_by" => {
                let field = str_arg(c);
                if field.is_empty() {
                    return Err("count_by: expected a field name".into());
                }
                ops.push(QueryOp::CountBy(field));
            }
            "min_by" => {
                let field = str_arg(c);
                if field.is_empty() {
                    return Err("min_by: expected a field name".into());
                }
                ops.push(QueryOp::MinBy(field));
            }
            "max_by" => {
                let field = str_arg(c);
                if field.is_empty() {
                    return Err("max_by: expected a field name".into());
                }
                ops.push(QueryOp::MaxBy(field));
            }
            "has" => {
                let key = str_arg(c);
                if key.is_empty() {
                    return Err("has: expected a key name".into());
                }
                ops.push(QueryOp::Has(key));
            }
            "entries" => {
                if !c.args.is_empty() { return Err("entries: takes no arguments".into()); }
                ops.push(QueryOp::Entries);
            }
            "flatten" => ops.push(QueryOp::Flatten),
            "add" => ops.push(QueryOp::Add),
            "over" => {
                let n = c
                    .args
                    .first()
                    .and_then(Arg::as_str)
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or_else(|| "over: expected a numeric threshold".to_string())?;
                ops.push(QueryOp::Over(n));
            }
            "under" => {
                let n: f64 = str_arg(c)
                    .parse()
                    .map_err(|_| "under: expected a number".to_string())?;
                ops.push(QueryOp::Under(n));
            }
            "between" => {
                let lo = c
                    .args
                    .first()
                    .and_then(Arg::as_str)
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or_else(|| "between: expected two numbers A B".to_string())?;
                let hi = c
                    .args
                    .get(1)
                    .and_then(Arg::as_str)
                    .and_then(|s| s.parse::<f64>().ok())
                    .ok_or_else(|| "between: expected two numbers A B".to_string())?;
                ops.push(QueryOp::Between(lo, hi));
            }
            "enumerate" => {
                ops.push(QueryOp::Enumerate);
            }
            "words" => {
                if !c.args.is_empty() { return Err("words: takes no arguments".into()); }
                ops.push(QueryOp::Words);
            }
            "dedup" => ops.push(QueryOp::Dedup),
            "tailn" => ops.push(QueryOp::Tailn(count_arg(c, "tailn")?)),
            "pad" => {
                let n = count_arg(c, "pad")?;
                ops.push(QueryOp::Pad(n));
            }
            "lpad" => ops.push(QueryOp::Lpad(count_arg(c, "lpad")?)),
            "grepf" => {
                let field = c
                    .args
                    .first()
                    .and_then(Arg::as_str)
                    .ok_or_else(|| "grepf: expected FIELD and /re/".to_string())?
                    .to_string();
                let raw = c
                    .args
                    .get(1)
                    .and_then(Arg::as_str)
                    .ok_or_else(|| "grepf: expected a pattern".to_string())?;
                let pat = raw
                    .strip_prefix('/')
                    .and_then(|s| s.strip_suffix('/'))
                    .unwrap_or(raw);
                let re = regex::Regex::new(pat).map_err(|e| format!("grepf: bad regex: {e}"))?;
                ops.push(QueryOp::Grepf(field, re));
            }
            "basename" => ops.push(QueryOp::Basename),
            "dirname" => ops.push(QueryOp::Dirname),
            "commafy" => ops.push(QueryOp::Commafy),
            "flip" => {
                ops.push(QueryOp::Flip);
            }
            "b64" => {
                ops.push(QueryOp::B64);
            }
            "b64d" => {
                ops.push(QueryOp::B64d);
            }
            "hex" => ops.push(QueryOp::Hex),
            "unhex" => {
                ops.push(QueryOp::Unhex);
            }
            "urlenc" => {
                ops.push(QueryOp::Urlenc);
            }
            "urldec" => {
                ops.push(QueryOp::Urldec);
            }
            "extract" => {
                ops.push(QueryOp::Extract(regex_arg(c)?));
            }
            "split" => {
                let delim = str_arg(c);
                if delim.is_empty() { return Err("split: expected a non-empty delimiter".into()); }
                ops.push(QueryOp::Split(delim));
            }
            "substr" => {
                let args: Vec<usize> = c
                    .args
                    .iter()
                    .filter_map(Arg::as_str)
                    .filter_map(|s| s.parse::<usize>().ok())
                    .collect();
                if args.len() != 2 {
                    return Err("substr: expected two non-negative integer args A B".into());
                }
                ops.push(QueryOp::Substr(args[0], args[1]));
            }
            "chars" => ops.push(QueryOp::Chars),
            "title" => {
                ops.push(QueryOp::Title);
            }
            "repeat" => {
                let n = count_arg(c, "repeat")?;
                ops.push(QueryOp::Repeat(n));
            }
            "set" => {
                let key = str_arg(c);
                if key.is_empty() { return Err("set: expected key and value".into()); }
                let val = c.args.iter().filter_map(Arg::as_str).nth(1).unwrap_or("").to_string();
                ops.push(QueryOp::Set(key, val));
            }
            "del" => {
                let key = str_arg(c);
                if key.is_empty() { return Err("del: expected a key name".into()); }
                ops.push(QueryOp::Del(key));
            }
            "rename" => {
                let args: Vec<String> = c.args.iter().filter_map(Arg::as_str).map(str::to_string).collect();
                if args.len() != 2 || args[0].is_empty() || args[1].is_empty() {
                    return Err("rename: expected OLD NEW key names".into());
                }
                ops.push(QueryOp::Rename(args[0].clone(), args[1].clone()));
            }
            "default" => {
                let args: Vec<String> = c.args.iter().filter_map(Arg::as_str).map(str::to_string).collect();
                if args.len() != 2 {
                    return Err("default: expected exactly two args: key value".into());
                }
                ops.push(QueryOp::Default(args[0].clone(), args[1].clone()));
            }
            "merge" => {
                ops.push(QueryOp::Merge);
            }
            "floor" => {
                ops.push(QueryOp::Floor);
            }
            "ceil" => {
                ops.push(QueryOp::Ceil);
            }
            "clamp" => {
                let mut it = c.args.iter().filter_map(Arg::as_str);
                let lo = it.next().and_then(|s| s.parse::<f64>().ok());
                let hi = it.next().and_then(|s| s.parse::<f64>().ok());
                match (lo, hi) {
                    (Some(lo), Some(hi)) => ops.push(QueryOp::Clamp(lo, hi)),
                    _ => return Err("clamp: expected LO HI numeric args".into()),
                }
            }
            "contains" => ops.push(QueryOp::Contains(str_arg(c))),
            "starts" => ops.push(QueryOp::Starts(str_arg(c))),
            "ends" => ops.push(QueryOp::Ends(str_arg(c))),
            "nonempty" => ops.push(QueryOp::Nonempty),
            "numeric" => ops.push(QueryOp::Numeric),
            "len" => ops.push(QueryOp::Len),
            "wc" => ops.push(QueryOp::Wc),
            "abs" => ops.push(QueryOp::Abs),
            "round" => ops.push(QueryOp::Round),
            "prepend" => ops.push(QueryOp::Prepend(str_arg(c))),
            "append" => ops.push(QueryOp::Append(str_arg(c))),
            "cut" => {
                let delim = str_arg(c);
                let n = c
                    .args
                    .get(1)
                    .and_then(Arg::as_str)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(0);
                ops.push(QueryOp::Cut(delim, n));
            }
            "median" => ops.push(QueryOp::Median),
            "stddev" => ops.push(QueryOp::Stddev),
            "p95" => ops.push(QueryOp::P95),
            "range" => ops.push(QueryOp::Range),
            "product" => ops.push(QueryOp::Product),
            "distinct" => ops.push(QueryOp::Distinct),
            "sample" => ops.push(QueryOp::Sample(count_arg(c, "sample")?)),
            "bins" => ops.push(QueryOp::Bins(count_arg(c, "bins")?)),
            "slice" => {
                let a = c
                    .args
                    .first()
                    .and_then(Arg::as_str)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(1);
                let b = c
                    .args
                    .get(1)
                    .and_then(Arg::as_str)
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(usize::MAX);
                ops.push(QueryOp::Slice(a, b));
            }
            other => return Err(format!("source: unknown verb `{other}`")),
        }
    }
    if !saw_in {
        return Err("source: pipeline must start with `in`".into());
    }
    Ok(ops)
}

/// Read a regex argument, stripping optional `/…/` delimiters.
fn regex_arg(c: &Command) -> Result<Regex, String> {
    let raw = c
        .args
        .first()
        .and_then(Arg::as_str)
        .ok_or_else(|| format!("{}: expected a pattern", c.name))?;
    let pat = raw
        .strip_prefix('/')
        .and_then(|s| s.strip_suffix('/'))
        .unwrap_or(raw);
    Regex::new(pat).map_err(|e| format!("{}: bad regex: {e}", c.name))
}

/// Parse a required count argument for `take`/`drop`.
/// The first arg as a string (empty if absent) — for verbs taking a literal.
fn str_arg(c: &Command) -> String {
    c.args
        .first()
        .and_then(Arg::as_str)
        .unwrap_or("")
        .to_string()
}

fn count_arg(c: &Command, verb: &str) -> Result<usize, String> {
    c.args
        .first()
        .and_then(Arg::as_str)
        .and_then(|s| s.parse::<usize>().ok())
        .ok_or_else(|| format!("{verb}: expected a count"))
}

/// A single numeric arg selects a whitespace column; anything else is a JSON
/// key path (`field a b c` -> a.b.c).
fn field_sel(args: &[Arg]) -> Result<FieldSel, String> {
    let words: Vec<&str> = args.iter().filter_map(Arg::as_str).collect();
    if words.is_empty() {
        return Err("field: expected a column number or key path".into());
    }
    if words.len() == 1 {
        if let Ok(n) = words[0].parse::<usize>() {
            return Ok(FieldSel::Col(n));
        }
    }
    Ok(FieldSel::Key(words.iter().map(|s| s.to_string()).collect()))
}

fn set_source(spec: &mut Spec, path: &str, src: Source) -> Result<(), String> {
    for w in &mut spec.widgets {
        if w.path == path {
            w.source = Some(src);
            return Ok(());
        }
    }
    Err(format!("source: no widget named `{path}`"))
}

fn set_grid(spec: &mut Spec, path: &str, cell: (usize, usize)) -> Result<(), String> {
    for w in &mut spec.widgets {
        if w.path == path {
            w.grid = Some(cell);
            return Ok(());
        }
    }
    Err(format!("grid: no widget named `{path}`"))
}
