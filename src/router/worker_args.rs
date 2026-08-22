use std::path::Path;

use super::RouterBootOptions;

pub(super) fn build(opts: &RouterBootOptions, home_dir: &Path) -> Vec<String> {
    let mut args = vec![
        "--bind".to_string(),
        opts.bind.clone(),
        "--home-dir".to_string(),
        home_dir.to_string_lossy().to_string(),
    ];
    if let Some(data_dir) = &opts.data_dir {
        args.push("--data-dir".to_string());
        args.push(data_dir.to_string_lossy().to_string());
    }
    if let Some(embeddings_dir) = &opts.embeddings_dir {
        args.push("--embeddings-dir".to_string());
        args.push(embeddings_dir.to_string_lossy().to_string());
    }
    // Cross-repo expansion: workers call back through the router to fetch
    // chunk content owned by OTHER repos' workers. 127.0.0.1 is correct even
    // when the router binds a wider interface — every component is same-host.
    if let Some(router_url) = &opts.router_url {
        args.push("--router-url".to_string());
        args.push(router_url.clone());
    }
    args
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn propagates_router_home_and_directory_overrides() {
        let mut opts = RouterBootOptions {
            data_dir: Some(PathBuf::from("data")),
            embeddings_dir: Some(PathBuf::from("embeddings")),
            bind: "127.0.0.1".to_string(),
            home_dir: None,
            worker_exe: None,
            router_url: None,
        };

        // router_url: None → no --router-url passthrough.
        assert_eq!(
            build(&opts, Path::new("home")),
            [
                "--bind",
                "127.0.0.1",
                "--home-dir",
                "home",
                "--data-dir",
                "data",
                "--embeddings-dir",
                "embeddings",
            ]
        );

        opts.router_url = Some("http://127.0.0.1:6699".to_string());
        assert_eq!(
            build(&opts, Path::new("home")),
            [
                "--bind",
                "127.0.0.1",
                "--home-dir",
                "home",
                "--data-dir",
                "data",
                "--embeddings-dir",
                "embeddings",
                "--router-url",
                "http://127.0.0.1:6699",
            ]
        );
    }
}
