use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use futures::stream;
use influxdb2::models::DataPoint;
use oauth2::basic::BasicClient;
use oauth2::http::HeaderValue;
use oauth2::reqwest::async_http_client;
use oauth2::{AccessToken, AuthUrl, ClientId, ClientSecret, TokenResponse, TokenUrl};
use reqwest::header;
use reqwest::ClientBuilder;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;

const ITEM_NAMES: &[u8] = include_bytes!("itemsparse.csv");

#[derive(Deserialize)]
struct Settings {
    influxdb: InfluxdbSettings,
    #[serde(rename = "battlenet")]
    battle_net: BlizzardSettings,
    #[serde(rename = "auctionhouses", default)]
    auction_houses: Vec<(i64, i64)>,
}

#[derive(Deserialize)]
struct InfluxdbSettings {
    host: String,
    org: String,
    token: AccessToken,
    bucket: String,
}

#[derive(Deserialize)]
struct BlizzardSettings {
    region: String,
    #[serde(rename = "clientid")]
    client_id: ClientId,
    #[serde(rename = "clientsecret")]
    client_secret: ClientSecret,
}

#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Args {
    /// Config file with influxdb and battle.net credentials.
    #[arg(short, long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Update all the prices once and then quit
    Update,

    /// List every available auction house and its realm
    ListAuctionHouses,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args: Args = Args::parse();
    let settings = get_settings(&args).context("Couldn't parse settings")?;

    let access_token = get_access_token(&settings.battle_net)
        .await
        .context("Couldn't authenticate with battle.net")?;

    match &args.command {
        Command::Update => {
            perform_single_update(&settings, access_token).await?;
        }
        Command::ListAuctionHouses => {
            list_all_auction_houses(&settings, access_token).await?;
        }
    }

    Ok(())
}

async fn perform_single_update(settings: &Settings, access_token: HeaderValue) -> Result<()> {
    let client = influxdb2::Client::new(
        &settings.influxdb.host,
        &settings.influxdb.org,
        settings.influxdb.token.secret(),
    );
    let names_by_id = read_names_by_id();

    for (realm, ah) in &settings.auction_houses {
        update_prices(
            &settings,
            &client,
            &names_by_id,
            access_token.clone(),
            *realm,
            *ah,
        )
        .await
        .context("Couldn't update price data")?;
    }

    println!("Done!");
    Ok(())
}

async fn list_all_auction_houses(settings: &Settings, access_token: HeaderValue) -> Result<()> {
    for connected_realm in get_connected_realms(settings, access_token.clone())
        .await?
        .connected_realms
    {
        let connected_realm =
            get_connected_realm(settings, access_token.clone(), connected_realm).await?;
        for realm in connected_realm.realms {
            println!("- {} -", realm.name);

            for auction_house in
                get_auction_houses(settings, access_token.clone(), connected_realm.id)
                    .await?
                    .auctions
            {
                println!(
                    "{} / {} - {}",
                    connected_realm.id, auction_house.id, auction_house.name
                );
            }
        }
    }
    Ok(())
}

fn get_settings(args: &Args) -> Result<Settings> {
    let mut settings = Figment::new();
    if let Some(path) = &args.config {
        settings = settings.merge(Toml::file(path));
    }
    settings = settings.merge(Env::prefixed("AH_").split("_"));
    Ok(settings.extract()?)
}

async fn update_prices(
    settings: &Settings,
    client: &influxdb2::Client,
    names_by_id: &HashMap<i64, String>,
    access_token: HeaderValue,
    realm: i64,
    ah: i64,
) -> Result<()> {
    let auctions = get_auctions(&settings, access_token, realm, ah)
        .await
        .context("Couldn't fetch list of auctions from battle.net")?
        .auctions;
    let mut by_items: HashMap<i64, ItemData> = HashMap::new();

    for auction in auctions {
        let mut entry = by_items.entry(auction.item.id).or_default();
        entry.auctions += 1;
        entry.total_items = entry.total_items.saturating_add(auction.quantity);
        if auction.buyout > 0 {
            let min_buyout = auction.buyout / auction.quantity;
            if entry.min_buyout == 0 || entry.min_buyout > min_buyout {
                entry.min_buyout = min_buyout;
            }
        }
    }

    let mut points = vec![];
    for (id, data) in by_items {
        let mut point = DataPoint::builder("auctions")
            .tag("item_id", id.to_string())
            .tag("realm_id", realm.to_string())
            .tag("ah_id", ah.to_string())
            .field("count", data.auctions)
            .field("total_items", data.total_items)
            .field("min_buyout", data.min_buyout);

        if let Some(name) = names_by_id.get(&id) {
            point = point.tag("item_name", name)
        }

        points.push(point.build()?);
    }

    client
        .write(&settings.influxdb.bucket, stream::iter(points))
        .await?;

    Ok(())
}

fn read_names_by_id() -> HashMap<i64, String> {
    let mut result = HashMap::new();
    let mut reader = csv::Reader::from_reader(ITEM_NAMES);

    for record in reader.records() {
        if let Ok(record) = record {
            if let (Some(id), Some(name)) = (
                record.get(0).and_then(|s| i64::from_str(s).ok()),
                record.get(6),
            ) {
                result.insert(id, name.to_string());
            }
        }
    }

    result
}

async fn get_auctions(
    settings: &Settings,
    access_token: HeaderValue,
    realm: i64,
    ah: i64,
) -> Result<AuctionList> {
    let mut headers = header::HeaderMap::new();
    headers.insert(header::AUTHORIZATION, access_token);
    headers.insert(
        "Battlenet-Namespace",
        header::HeaderValue::from_str(&format!("dynamic-classic-{}", settings.battle_net.region))?,
    );
    let client = ClientBuilder::new().default_headers(headers).build()?;

    println!("Requesting auctions for realm {} AH {}...", realm, ah);
    Ok(client
        .get(&format!(
            "https://{}.api.blizzard.com/data/wow/connected-realm/{}/auctions/{}",
            settings.battle_net.region, realm, ah
        ))
        .send()
        .await
        .context("Couldn't submit request for auction house data")?
        .json::<AuctionList>()
        .await
        .context("Couldn't parse auction house data")?)
}

async fn get_connected_realms(
    settings: &Settings,
    access_token: HeaderValue,
) -> Result<ConnectedRealmList> {
    let mut headers = header::HeaderMap::new();
    headers.insert(header::AUTHORIZATION, access_token);
    headers.insert(
        "Battlenet-Namespace",
        header::HeaderValue::from_str(&format!("dynamic-classic-{}", settings.battle_net.region))?,
    );
    let client = ClientBuilder::new().default_headers(headers).build()?;

    Ok(client
        .get(&format!(
            "https://{}.api.blizzard.com/data/wow/connected-realm/index",
            settings.battle_net.region
        ))
        .send()
        .await
        .context("Couldn't submit request for connected realm list")?
        .json::<ConnectedRealmList>()
        .await
        .context("Couldn't parse connected realm list")?)
}

async fn get_connected_realm(
    settings: &Settings,
    access_token: HeaderValue,
    link: ConnectedRealmLink,
) -> Result<ConnectedRealm> {
    let mut headers = header::HeaderMap::new();
    headers.insert(header::AUTHORIZATION, access_token);
    headers.insert(
        "Battlenet-Namespace",
        header::HeaderValue::from_str(&format!("dynamic-classic-{}", settings.battle_net.region))?,
    );
    let client = ClientBuilder::new().default_headers(headers).build()?;

    Ok(client
        .get(link.href)
        .query(&[("locale", "en_US")])
        .send()
        .await
        .context("Couldn't submit request for connected realm")?
        .json::<ConnectedRealm>()
        .await
        .context("Couldn't parse connected realm")?)
}

async fn get_auction_houses(
    settings: &Settings,
    access_token: HeaderValue,
    realm: i64,
) -> Result<AuctionHouseList> {
    let mut headers = header::HeaderMap::new();
    headers.insert(header::AUTHORIZATION, access_token);
    headers.insert(
        "Battlenet-Namespace",
        header::HeaderValue::from_str(&format!("dynamic-classic-{}", settings.battle_net.region))?,
    );
    let client = ClientBuilder::new().default_headers(headers).build()?;

    Ok(client
        .get(&format!(
            "https://{}.api.blizzard.com/data/wow/connected-realm/{}/auctions/index",
            settings.battle_net.region, realm,
        ))
        .query(&[("locale", "en_US")])
        .send()
        .await
        .context("Couldn't submit request for auction house index")?
        .json::<AuctionHouseList>()
        .await
        .context("Couldn't parse auction house index")?)
}

async fn get_access_token(settings: &BlizzardSettings) -> Result<header::HeaderValue> {
    let client = BasicClient::new(
        settings.client_id.clone(),
        Some(settings.client_secret.clone()),
        AuthUrl::new("http://localhost:8080".to_string())?,
        Some(TokenUrl::new("https://oauth.battle.net/token".to_string())?),
    );

    println!("Authenticating...");
    let result = client
        .exchange_client_credentials()
        .request_async(async_http_client)
        .await?;
    let mut value = header::HeaderValue::from_str(&format!(
        "{:?} {}",
        result.token_type(),
        result.access_token().secret()
    ))?;
    value.set_sensitive(true);

    Ok(value)
}

#[derive(Serialize, Deserialize, Debug)]
struct AuctionHouseList {
    pub auctions: Vec<AuctionHouse>,
}

#[derive(Serialize, Deserialize, Debug)]
struct AuctionHouse {
    pub id: i64,
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct ConnectedRealmList {
    pub connected_realms: Vec<ConnectedRealmLink>,
}

#[derive(Serialize, Deserialize, Debug)]
struct ConnectedRealmLink {
    pub href: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct ConnectedRealm {
    pub id: i64,
    pub realms: Vec<Realm>,
}

#[derive(Serialize, Deserialize, Debug)]
struct Realm {
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct Item {
    pub id: i64,
    pub rand: Option<i64>,
    pub seed: Option<i64>,
}

#[derive(Serialize, Deserialize, Debug)]
struct Auction {
    pub id: i64,
    pub item: Item,
    pub bid: i64,
    pub buyout: i64,
    pub quantity: i64,
    pub time_left: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct AuctionList {
    pub auctions: Vec<Auction>,
}

#[derive(Debug, Default)]
struct ItemData {
    auctions: i64,
    total_items: i64,
    min_buyout: i64,
}
