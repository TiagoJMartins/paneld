//! No configuration the validator accepts may panic the renderer.
//!
//! This defends a real defect class rather than a hypothetical one: the text
//! layout engine asserts internally on a cell whose padding exceeds its content
//! box, and a panic in the render task would kill the loop while the HTTP
//! listener went on serving a stale frame indefinitely. A 1x1 panel reached it.

use paneld::config::MIN_CELL;

#[tokio::test]
async fn no_valid_geometry_panics_the_renderer() {
    let mut checked = 0;
    let mut failures = Vec::new();

    for (cols, rows) in [(1u32, 1u32), (2, 2), (4, 3), (8, 6), (3, 1), (1, 4)] {
        for scale in [1u32, 2, 6] {
            let (width, height) = (MIN_CELL * cols * scale, MIN_CELL * rows * scale);
            if width > 4096 || height > 4096 {
                continue;
            }
            let mut widgets = String::new();
            for row in 0..rows {
                for col in 0..cols {
                    widgets.push_str(&format!(
                        "\n[[device.widget]]\nid = \"w{row}_{col}\"\nkind = \"value\"\ncol = {col}\nrow = {row}\nlabel = \"A Longish Label\"\nunit = \"kWh\"\n"
                    ));
                }
            }
            let toml = format!(
                r#"
[server]
listen = "0.0.0.0:4444"
public_base_url = "http://192.168.0.50:4444"

[[device]]
id = "probe"
width = {width}
height = {height}
palette = "gray16"
dither = "bayer"
refresh_rate = 300
grid = {{ cols = {cols}, rows = {rows} }}
{widgets}"#
            );

            let config = match paneld::config::parse(&toml) {
                Ok(config) => config,
                Err(_) => continue,
            };
            let (runtime, _wake) = paneld::app::Runtime::with_home_assistant(config, None).unwrap();
            for widget in 0..(cols * rows) {
                let row = widget / cols;
                let col = widget % cols;
                let body = paneld::content::ContentBody {
                    value: Some(Some(serde_json::json!(123_456))),
                    state: None,
                    unit: None,
                    rows: None,
                    render: false,
                };
                runtime
                    .content
                    .put(
                        &format!("w{row}_{col}"),
                        body,
                        time::OffsetDateTime::now_utc(),
                    )
                    .unwrap();
            }

            checked += 1;
            if let Err(error) = runtime
                .render_device("probe", time::OffsetDateTime::now_utc())
                .await
            {
                failures.push(format!("{width}x{height} {cols}x{rows}: {error:#}"));
            }
        }
    }

    println!("checked {checked} geometries");
    assert!(failures.is_empty(), "failures: {failures:#?}");
}
