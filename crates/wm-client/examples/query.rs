// Modified by kdnk for wm on 2026-11-14. Original attribution is preserved in LICENSE.
use std::error::Error;

use wm_client::WmMachClient;

fn main() -> Result<(), Box<dyn Error>> {
    let client = WmMachClient::connect()?;
    let data = client.get_workspaces(None)?;

    println!("{}", serde_json::to_string_pretty(&data)?);
    Ok(())
}
