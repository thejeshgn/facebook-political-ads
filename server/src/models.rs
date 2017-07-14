use diesel;
use diesel::prelude::*;
use diesel::pg::PgConnection;
use diesel::pg::upsert::*;
use r2d2_diesel::ConnectionManager;
use r2d2::Pool;
use super::InsertError;
use super::schema::ads;
use super::server::AdPost;
use kuchiki;
use kuchiki::traits::*;

#[derive(Queryable, Debug)]
pub struct Ad {
    pub id: String,
    pub html: String,
    pub political: i32,
    pub not_political: i32,

    pub fuzzy_id: Option<i32>,
    pub title: Option<String>,
    pub message: Option<String>,
    pub image: Option<String>,
    pub big_image: Option<String>,
}

#[derive(Insertable)]
#[table_name = "ads"]
pub struct NewAd<'a> {
    pub id: &'a str,
    pub html: &'a str,
    pub political: i32,
    pub not_political: i32,
    pub title: Option<String>,
    pub message: Option<String>,
    pub image: Option<String>,
}

impl<'a> NewAd<'a> {
    pub fn new(ad: &'a AdPost) -> Result<NewAd<'a>, InsertError> {
        let document = kuchiki::parse_html().one(ad.html.clone());

        let message = document
            .select(".userContent p")
            .or(document.select("span"))
            .map_err(InsertError::HTML)?
            .nth(0)
            .and_then(|a| Some(a.as_node().to_string()));

        let title = document
            .select("h5 a")
            .or(document.select("h6 a"))
            .or(document.select("strong"))
            .map_err(InsertError::HTML)?
            .nth(0)
            .and_then(|a| Some(a.text_contents()));

        let img = document
            .select("img")
            .map_err(InsertError::HTML)?
            .nth(0)
            .and_then(|a| {
                a.attributes.borrow().get("src").and_then(
                    |src| Some(src.to_string()),
                )
            });

        Ok(NewAd {
            id: &ad.id,
            html: &ad.html,
            political: if ad.political { 1 } else { 0 },
            not_political: if !ad.political { 1 } else { 0 },
            title: title,
            message: message,
            image: img,
        })
    }

    pub fn save(&self, pool: &Pool<ConnectionManager<PgConnection>>) -> Result<Ad, InsertError> {
        use schema::ads;
        use schema::ads::dsl::*;
        let connection = pool.get().map_err(InsertError::Timeout)?;
        let ad: Ad = diesel::insert(&self.on_conflict(
            id,
            do_update().set((
                political.eq(political + self.political),
                not_political.eq(
                    not_political +
                        self.not_political,
                ),
            )),
        )).into(ads::table)
            .get_result(&*connection)
            .map_err(InsertError::DataBase)?;
        Ok(ad)
    }
}
