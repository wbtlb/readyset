#![feature(async_closure)]

extern crate chrono;
extern crate mysql;
extern crate mysql_common as myc;
extern crate mysql_srv;
extern crate nom;
extern crate tokio;

use core::iter;
use std::collections::HashMap;
use std::future::Future;
use std::marker::PhantomData;
use std::pin::Pin;
use std::{io, net, thread};

use async_trait::async_trait;
use mysql::prelude::Queryable;
use mysql::Row;
use mysql_srv::{
    CachedSchema, Column, ErrorKind, InitWriter, MySqlIntermediary, MySqlShim, ParamParser,
    QueryResultWriter, StatementMetaWriter,
};
use tokio::io::AsyncWrite;
use tokio::net::tcp::OwnedWriteHalf;

static DEFAULT_CHARACTER_SET: u16 = myc::constants::UTF8_GENERAL_CI;

struct TestingShim<Q, P, E, I, W> {
    columns: Vec<Column>,
    params: Vec<Column>,
    on_q: Q,
    on_p: P,
    on_e: E,
    on_i: I,
    _phantom: PhantomData<W>,
}

#[async_trait]
impl<Q, P, E, I, W> MySqlShim<W> for TestingShim<Q, P, E, I, W>
where
    Q: for<'a> FnMut(
            &'a str,
            QueryResultWriter<'a, W>,
        ) -> Pin<Box<dyn Future<Output = io::Result<()>> + 'a + Send>>
        + Send,
    P: FnMut(&str) -> u32 + Send,
    E: for<'a> FnMut(
            u32,
            Vec<mysql_srv::ParamValue>,
            QueryResultWriter<'a, W>,
        ) -> Pin<Box<dyn Future<Output = io::Result<()>> + 'a + Send>>
        + Send,
    I: for<'a> FnMut(
            &'a str,
            InitWriter<'a, W>,
        ) -> Pin<Box<dyn Future<Output = io::Result<()>> + 'a + Send>>
        + Send,
    W: AsyncWrite + Unpin + Send + 'static,
{
    async fn on_prepare(
        &mut self,
        query: &str,
        info: StatementMetaWriter<'_, W>,
        _schema_cache: &mut HashMap<u32, CachedSchema>,
    ) -> io::Result<()> {
        let id = (self.on_p)(query);
        info.reply(id, &self.params, &self.columns).await
    }

    async fn on_execute(
        &mut self,
        id: u32,
        params: ParamParser<'_>,
        results: QueryResultWriter<'_, W>,
        _schema_cache: &mut HashMap<u32, CachedSchema>,
    ) -> io::Result<()> {
        let mut extract_params = Vec::new();
        for p in params {
            extract_params.push(p.map_err(|e| {
                let e: std::io::Error = e.into();
                e
            })?);
        }
        (self.on_e)(id, extract_params, results).await
    }

    async fn on_close(&mut self, _: u32) {}

    async fn on_init(&mut self, schema: &str, writer: InitWriter<'_, W>) -> io::Result<()> {
        (self.on_i)(schema, writer).await
    }

    async fn on_query(&mut self, query: &str, results: QueryResultWriter<'_, W>) -> io::Result<()> {
        if query.starts_with("SELECT @@") || query.starts_with("select @@") {
            let var = &query.get(b"SELECT @@".len()..);
            return match var {
                Some("max_allowed_packet") => {
                    let cols = &[Column {
                        table: String::new(),
                        column: "@@max_allowed_packet".to_owned(),
                        coltype: myc::constants::ColumnType::MYSQL_TYPE_LONG,
                        column_length: None,
                        colflags: myc::constants::ColumnFlags::UNSIGNED_FLAG,
                        character_set: DEFAULT_CHARACTER_SET,
                    }];
                    let mut w = results.start(cols).await?;
                    w.write_row(iter::once(67108864u32)).await?;
                    Ok(w.finish().await?)
                }
                _ => Ok(results.completed(0, 0, None).await?),
            };
        } else {
            (self.on_q)(query, results).await
        }
    }

    fn password_for_username(&self, username: &str) -> Option<Vec<u8>> {
        if username == "user" {
            Some(b"password".to_vec())
        } else {
            None
        }
    }
}

impl<Q, P, E, I> TestingShim<Q, P, E, I, OwnedWriteHalf>
where
    Q: for<'a> FnMut(
            &'a str,
            QueryResultWriter<'a, OwnedWriteHalf>,
        ) -> Pin<Box<dyn Future<Output = io::Result<()>> + 'a + Send>>
        + Send
        + 'static,
    P: FnMut(&str) -> u32 + Send + 'static,
    E: for<'a> FnMut(
            u32,
            Vec<mysql_srv::ParamValue>,
            QueryResultWriter<'a, OwnedWriteHalf>,
        ) -> Pin<Box<dyn Future<Output = io::Result<()>> + 'a + Send>>
        + Send
        + 'static,
    I: for<'a> FnMut(
            &'a str,
            InitWriter<'a, OwnedWriteHalf>,
        ) -> Pin<Box<dyn Future<Output = io::Result<()>> + 'a + Send>>
        + Send
        + 'static,
{
    fn new(on_q: Q, on_p: P, on_e: E, on_i: I) -> Self {
        TestingShim {
            columns: Vec::new(),
            params: Vec::new(),
            on_q,
            on_p,
            on_e,
            on_i,
            _phantom: PhantomData,
        }
    }

    fn with_params(mut self, p: Vec<Column>) -> Self {
        self.params = p;
        self
    }

    fn with_columns(mut self, c: Vec<Column>) -> Self {
        self.columns = c;
        self
    }

    fn test<C>(self, c: C)
    where
        C: FnOnce(&mut mysql::Conn),
    {
        let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let port = listener.local_addr().unwrap().port();
        let jh = thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let s = {
                let _guard = rt.handle().enter();
                tokio::net::TcpStream::from_std(s).unwrap()
            };
            rt.block_on(MySqlIntermediary::run_on_tcp(self, s))
        });

        let mut db = mysql::Conn::new(
            mysql::Opts::from_url(&format!("mysql://user:password@127.0.0.1:{}", port)).unwrap(),
        )
        .unwrap();
        c(&mut db);
        drop(db);
        jh.join().unwrap().unwrap();
    }
}

#[test]
fn it_connects() {
    TestingShim::new(
        move |_, _| unreachable!(),
        move |_| unreachable!(),
        move |_, _, _| unreachable!(),
        move |_, _| unreachable!(),
    )
    .test(|_| {})
}

/*
#[test]
fn failed_authentication() {
    let shim = TestingShim::new(
        |_, _| unreachable!(),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    );
    let listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let jh = thread::spawn(move || {
        let (s, _) = listener.accept().unwrap();
        MySqlIntermediary::run_on_tcp(shim, s)
    });

    let res = mysql::Conn::new(&format!("mysql://user:bad_password@127.0.0.1:{}", port));
    assert!(res.is_err());
    match res.err().unwrap() {
        mysql::Error::MySqlError(err) => {
            assert_eq!(err.code, u16::from(ErrorKind::ER_ACCESS_DENIED_ERROR));
            assert_eq!(err.message, "Access denied for user user".to_owned());
        }
        err => panic!("Not a mysql error: {:?}", err),
    }

    jh.join().unwrap().unwrap();
}
 */
#[test]
fn it_inits_ok() {
    TestingShim::new(
        |_, _| unreachable!(),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |schema, writer| {
            assert_eq!(schema, "test");
            Box::pin(async move { writer.ok().await })
        },
    )
    .test(|db| assert!(db.select_db("test")));
}

#[test]
fn it_inits_error() {
    TestingShim::new(
        |_, _| unreachable!(),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |schema, writer| {
            assert_eq!(schema, "test");
            Box::pin(async move {
                writer
                    .error(
                        ErrorKind::ER_BAD_DB_ERROR,
                        format!("Database {} not found", schema).as_bytes(),
                    )
                    .await
            })
        },
    )
    .test(|db| assert!(!db.select_db("test")));
}

#[test]
fn it_pings() {
    TestingShim::new(
        |_, _| unreachable!(),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| assert!(db.ping()))
}

#[test]
fn empty_response() {
    TestingShim::new(
        |_, w| Box::pin(async move { w.completed(0, 0, None).await }),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        assert_eq!(db.query::<Row, _>("SELECT a, b FROM foo").unwrap().len(), 0);
    })
}

#[test]
fn no_rows() {
    let cols = [Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        column_length: None,
        colflags: myc::constants::ColumnFlags::empty(),
        character_set: DEFAULT_CHARACTER_SET,
    }];
    TestingShim::new(
        move |_, w| {
            let cols = cols.clone();
            Box::pin(async move { w.start(&cols[..]).await?.finish().await })
        },
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        assert_eq!(db.query::<Row, _>("SELECT a, b FROM foo").unwrap().len(), 0);
    })
}

#[test]
fn no_columns() {
    TestingShim::new(
        move |_, w| Box::pin(async move { w.start(&[]).await?.finish().await }),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        assert_eq!(db.query::<Row, _>("SELECT a, b FROM foo").unwrap().len(), 0);
    })
}

#[test]
fn no_columns_but_rows() {
    TestingShim::new(
        move |_, w| {
            Box::pin(async move {
                let mut w = w.start(&[]).await?;
                w.write_col(42i32)?;
                w.finish().await
            })
        },
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        assert_eq!(db.query::<Row, _>("SELECT a, b FROM foo").unwrap().len(), 0);
    })
}

#[test]
fn error_response() {
    let err = (ErrorKind::ER_NO, "clearly not");
    TestingShim::new(
        move |_, w| Box::pin(async move { w.error(err.0, err.1.as_bytes()).await }),
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        if let mysql::Error::MySqlError(e) = db.query::<Row, _>("SELECT a, b FROM foo").unwrap_err()
        {
            assert_eq!(
                e,
                mysql::error::MySqlError {
                    state: String::from_utf8(err.0.sqlstate().to_vec()).unwrap(),
                    message: err.1.to_owned(),
                    code: err.0 as u16,
                }
            );
        } else {
            unreachable!();
        }
    })
}

#[test]
fn it_queries_nulls() {
    TestingShim::new(
        |_, w| {
            let cols = [Column {
                table: String::new(),
                column: "a".to_owned(),
                coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
                column_length: None,
                colflags: myc::constants::ColumnFlags::empty(),
                character_set: DEFAULT_CHARACTER_SET,
            }];
            Box::pin(async move {
                let mut w = w.start(&cols).await?;
                w.write_col(None::<i16>)?;
                w.finish().await
            })
        },
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        let res = db.query::<Row, _>("SELECT a, b FROM foo").unwrap();
        let row = res.first().unwrap();
        assert_eq!(row.get(0), Some(mysql::Value::NULL));
    })
}

#[test]
fn it_queries() {
    TestingShim::new(
        |_, w| {
            let cols = [Column {
                table: String::new(),
                column: "a".to_owned(),
                coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
                column_length: None,
                colflags: myc::constants::ColumnFlags::empty(),
                character_set: DEFAULT_CHARACTER_SET,
            }];
            Box::pin(async move {
                let mut w = w.start(&cols).await?;
                w.write_col(1024i16)?;
                w.finish().await
            })
        },
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        let res = db.query::<Row, _>("SELECT a, b FROM foo").unwrap();
        let row = res.first().unwrap();
        assert_eq!(row.get(0), Some(1024));
    })
}

#[test]
fn multi_result() {
    TestingShim::new(
        |_, w| {
            let cols = [Column {
                table: String::new(),
                column: "a".to_owned(),
                coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
                column_length: None,
                colflags: myc::constants::ColumnFlags::empty(),
                character_set: DEFAULT_CHARACTER_SET,
            }];
            Box::pin(async move {
                let mut row = w.start(&cols).await?;
                row.write_col(1024i16)?;
                let w = row.finish_one().await?;
                let mut row = w.start(&cols).await?;
                row.write_col(1025i16)?;
                row.finish().await
            })
        },
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        let mut result = db
            .query_iter("SELECT a FROM foo; SELECT a FROM foo")
            .unwrap();
        let mut set1 = result.iter().unwrap();
        let row1 = set1.next().unwrap().unwrap();
        assert_eq!(row1.get::<i16, _>(0), Some(1024));
        drop(set1);
        let mut set2 = result.iter().unwrap();
        let row2 = set2.next().unwrap().unwrap();
        assert_eq!(row2.get::<i16, _>(0), Some(1025));
    })
}

#[test]
fn it_queries_many_rows() {
    TestingShim::new(
        |_, w| {
            let cols = [
                Column {
                    table: String::new(),
                    column: "a".to_owned(),
                    coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
                    column_length: None,
                    colflags: myc::constants::ColumnFlags::empty(),
                    character_set: DEFAULT_CHARACTER_SET,
                },
                Column {
                    table: String::new(),
                    column: "b".to_owned(),
                    coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
                    column_length: None,
                    colflags: myc::constants::ColumnFlags::empty(),
                    character_set: DEFAULT_CHARACTER_SET,
                },
            ];
            Box::pin(async move {
                let mut w = w.start(&cols).await?;
                w.write_col(1024i16)?;
                w.write_col(1025i16)?;
                w.end_row().await?;
                w.write_row(&[1024i16, 1025i16]).await?;
                w.finish().await
            })
        },
        |_| unreachable!(),
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(|db| {
        let mut rows = 0;
        for row in db.query_iter("SELECT a, b FROM foo").unwrap() {
            let row = row.unwrap();
            assert_eq!(row.get::<i16, _>(0), Some(1024));
            assert_eq!(row.get::<i16, _>(1), Some(1025));
            rows += 1;
        }
        assert_eq!(rows, 2);
    })
}

#[test]
fn it_prepares() {
    let cols = vec![Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        column_length: None,
        colflags: myc::constants::ColumnFlags::empty(),
        character_set: DEFAULT_CHARACTER_SET,
    }];
    let cols2 = cols.clone();
    let params = vec![Column {
        table: String::new(),
        column: "c".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        column_length: None,
        colflags: myc::constants::ColumnFlags::empty(),
        character_set: DEFAULT_CHARACTER_SET,
    }];

    TestingShim::new(
        |_, _| unreachable!(),
        |q| {
            assert_eq!(q, "SELECT a FROM b WHERE c = ?");
            41
        },
        move |stmt, params, w| {
            assert_eq!(stmt, 41);
            assert_eq!(params.len(), 1);
            // rust-mysql sends all numbers as LONGLONG
            assert_eq!(
                params[0].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_LONGLONG
            );
            assert_eq!(
                std::convert::TryInto::<i8>::try_into(params[0].value)
                    .expect("Error calling try_into"),
                42i8
            );

            let cols = cols.clone();
            Box::pin(async move {
                let mut w = w.start(&cols).await?;
                w.write_col(1024i16)?;
                w.finish().await
            })
        },
        |_, _| unreachable!(),
    )
    .with_params(params)
    .with_columns(cols2)
    .test(|db| {
        let res = db
            .exec::<Row, _, _>("SELECT a FROM b WHERE c = ?", (42i16,))
            .unwrap();
        let row = res.first().unwrap();
        assert_eq!(row.get::<i16, _>(0), Some(1024i16));
    })
}

#[test]
fn insert_exec() {
    let params = vec![
        Column {
            table: String::new(),
            column: "username".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            column_length: None,
            colflags: myc::constants::ColumnFlags::empty(),
            character_set: DEFAULT_CHARACTER_SET,
        },
        Column {
            table: String::new(),
            column: "email".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            column_length: None,
            colflags: myc::constants::ColumnFlags::empty(),
            character_set: DEFAULT_CHARACTER_SET,
        },
        Column {
            table: String::new(),
            column: "pw".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            column_length: None,
            colflags: myc::constants::ColumnFlags::empty(),
            character_set: DEFAULT_CHARACTER_SET,
        },
        Column {
            table: String::new(),
            column: "created".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_DATETIME,
            column_length: None,
            colflags: myc::constants::ColumnFlags::empty(),
            character_set: DEFAULT_CHARACTER_SET,
        },
        Column {
            table: String::new(),
            column: "session".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            column_length: None,
            colflags: myc::constants::ColumnFlags::empty(),
            character_set: DEFAULT_CHARACTER_SET,
        },
        Column {
            table: String::new(),
            column: "rss".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            column_length: None,
            colflags: myc::constants::ColumnFlags::empty(),
            character_set: DEFAULT_CHARACTER_SET,
        },
        Column {
            table: String::new(),
            column: "mail".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_VARCHAR,
            column_length: None,
            colflags: myc::constants::ColumnFlags::empty(),
            character_set: DEFAULT_CHARACTER_SET,
        },
    ];

    TestingShim::new(
        |_, _| unreachable!(),
        |_| 1,
        move |_, params, w| {
            assert_eq!(params.len(), 7);
            assert_eq!(
                params[0].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                params[1].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                params[2].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                params[3].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_DATETIME
            );
            assert_eq!(
                params[4].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                params[5].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                params[6].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                std::convert::TryInto::<&str>::try_into(params[0].value)
                    .expect("Error calling try_into"),
                "user199"
            );
            assert_eq!(
                std::convert::TryInto::<&str>::try_into(params[1].value)
                    .expect("Error calling try_into"),
                "user199@example.com"
            );
            assert_eq!(
                std::convert::TryInto::<&str>::try_into(params[2].value)
                    .expect("Error calling try_into"),
                "$2a$10$Tq3wrGeC0xtgzuxqOlc3v.07VTUvxvwI70kuoVihoO2cE5qj7ooka"
            );
            assert_eq!(
                std::convert::TryInto::<chrono::NaiveDateTime>::try_into(params[3].value)
                    .expect("Error calling try_into"),
                chrono::NaiveDate::from_ymd(2018, 4, 6).and_hms(13, 0, 56)
            );
            assert_eq!(
                std::convert::TryInto::<&str>::try_into(params[4].value)
                    .expect("Error calling try_into"),
                "token199"
            );
            assert_eq!(
                std::convert::TryInto::<&str>::try_into(params[5].value)
                    .expect("Error calling try_into"),
                "rsstoken199"
            );
            assert_eq!(
                std::convert::TryInto::<&str>::try_into(params[6].value)
                    .expect("Error calling try_into"),
                "mtok199"
            );

            Box::pin(async move { w.completed(42, 1, None).await })
        },
        |_, _| unreachable!(),
    )
    .with_params(params)
    .test(|db| {
        db.exec::<Row, _, _>(
            "INSERT INTO `users` \
                 (`username`, `email`, `password_digest`, `created_at`, \
                 `session_token`, `rss_token`, `mailing_list_token`) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
            (
                "user199",
                "user199@example.com",
                "$2a$10$Tq3wrGeC0xtgzuxqOlc3v.07VTUvxvwI70kuoVihoO2cE5qj7ooka",
                mysql::Value::Date(2018, 4, 6, 13, 0, 56, 0),
                "token199",
                "rsstoken199",
                "mtok199",
            ),
        )
        .unwrap();
        assert_eq!(db.affected_rows(), 42);
        assert_eq!(db.last_insert_id(), 1);
    })
}

#[test]
fn send_long() {
    let cols = vec![Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        column_length: None,
        colflags: myc::constants::ColumnFlags::empty(),
        character_set: DEFAULT_CHARACTER_SET,
    }];
    let cols2 = cols.clone();
    let params = vec![Column {
        table: String::new(),
        column: "c".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_BLOB,
        column_length: None,
        colflags: myc::constants::ColumnFlags::empty(),
        character_set: DEFAULT_CHARACTER_SET,
    }];

    TestingShim::new(
        |_, _| unreachable!(),
        |q| {
            assert_eq!(q, "SELECT a FROM b WHERE c = ?");
            41
        },
        move |stmt, params, w| {
            assert_eq!(stmt, 41);
            assert_eq!(params.len(), 1);
            // rust-mysql sends all strings as VAR_STRING
            assert_eq!(
                params[0].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_VAR_STRING
            );
            assert_eq!(
                std::convert::TryInto::<&[u8]>::try_into(params[0].value)
                    .expect("Error calling try_into"),
                b"Hello world"
            );

            let cols = cols.clone();
            Box::pin(async move {
                let mut w = w.start(&cols).await?;
                w.write_col(1024i16)?;
                w.finish().await
            })
        },
        |_, _| unreachable!(),
    )
    .with_params(params)
    .with_columns(cols2)
    .test(|db| {
        let res = db
            .exec::<Row, _, _>("SELECT a FROM b WHERE c = ?", (b"Hello world",))
            .unwrap();
        let row = res.first().unwrap();
        assert_eq!(row.get::<i16, _>(0), Some(1024i16));
    })
}

#[test]
fn it_prepares_many() {
    let cols = vec![
        Column {
            table: String::new(),
            column: "a".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            column_length: None,
            colflags: myc::constants::ColumnFlags::empty(),
            character_set: DEFAULT_CHARACTER_SET,
        },
        Column {
            table: String::new(),
            column: "b".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            column_length: None,
            colflags: myc::constants::ColumnFlags::empty(),
            character_set: DEFAULT_CHARACTER_SET,
        },
    ];
    let cols2 = cols.clone();

    TestingShim::new(
        |_, _| unreachable!(),
        |q| {
            assert_eq!(q, "SELECT a, b FROM x");
            41
        },
        move |stmt, params, w| {
            assert_eq!(stmt, 41);
            assert_eq!(params.len(), 0);

            let cols = cols.clone();
            Box::pin(async move {
                let mut w = w.start(&cols).await?;
                w.write_col(1024i16)?;
                w.write_col(1025i16)?;
                w.end_row().await?;
                w.write_row(&[1024i16, 1025i16]).await?;
                w.finish().await
            })
        },
        |_, _| unreachable!(),
    )
    .with_params(Vec::new())
    .with_columns(cols2)
    .test(|db| {
        let mut rows = 0;
        for row in db.exec::<Row, _, _>("SELECT a, b FROM x", ()).unwrap() {
            assert_eq!(row.get::<i16, _>(0), Some(1024));
            assert_eq!(row.get::<i16, _>(1), Some(1025));
            rows += 1;
        }
        assert_eq!(rows, 2);
    })
}

#[test]
fn prepared_empty() {
    let cols = vec![Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        column_length: None,
        colflags: myc::constants::ColumnFlags::empty(),
        character_set: DEFAULT_CHARACTER_SET,
    }];
    let cols2 = cols;
    let params = vec![Column {
        table: String::new(),
        column: "c".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        column_length: None,
        colflags: myc::constants::ColumnFlags::empty(),
        character_set: DEFAULT_CHARACTER_SET,
    }];

    TestingShim::new(
        |_, _| unreachable!(),
        |_| 0,
        move |_, params, w| {
            assert!(!params.is_empty());
            Box::pin(async move { w.completed(0, 0, None).await })
        },
        |_, _| unreachable!(),
    )
    .with_params(params)
    .with_columns(cols2)
    .test(|db| {
        assert_eq!(
            db.exec::<Row, _, _>("SELECT a FROM b WHERE c = ?", (42i16,))
                .unwrap()
                .len(),
            0
        );
    })
}

#[test]
fn prepared_no_params() {
    let cols = vec![Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        column_length: None,
        colflags: myc::constants::ColumnFlags::empty(),
        character_set: DEFAULT_CHARACTER_SET,
    }];
    let cols2 = cols.clone();
    let params = vec![];

    TestingShim::new(
        |_, _| unreachable!(),
        |_| 0,
        move |_, params, w| {
            assert!(params.is_empty());
            let cols = cols.clone();
            Box::pin(async move {
                let mut w = w.start(&cols).await?;
                w.write_col(1024i16)?;
                w.finish().await
            })
        },
        |_, _| unreachable!(),
    )
    .with_params(params)
    .with_columns(cols2)
    .test(|db| {
        let res = db.exec::<Row, _, _>("foo", ()).unwrap();
        let row = res.first().unwrap();
        assert_eq!(row.get::<i16, _>(0), Some(1024i16));
    })
}

#[test]
fn prepared_nulls() {
    let cols = vec![
        Column {
            table: String::new(),
            column: "a".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            column_length: None,
            colflags: myc::constants::ColumnFlags::empty(),
            character_set: DEFAULT_CHARACTER_SET,
        },
        Column {
            table: String::new(),
            column: "b".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            column_length: None,
            colflags: myc::constants::ColumnFlags::empty(),
            character_set: DEFAULT_CHARACTER_SET,
        },
    ];
    let cols2 = cols.clone();
    let params = vec![
        Column {
            table: String::new(),
            column: "c".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            column_length: None,
            colflags: myc::constants::ColumnFlags::empty(),
            character_set: DEFAULT_CHARACTER_SET,
        },
        Column {
            table: String::new(),
            column: "d".to_owned(),
            coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
            column_length: None,
            colflags: myc::constants::ColumnFlags::empty(),
            character_set: DEFAULT_CHARACTER_SET,
        },
    ];

    TestingShim::new(
        |_, _| unreachable!(),
        |_| 0,
        move |_, params, w| {
            assert_eq!(params.len(), 2);
            assert!(params[0].value.is_null());
            assert!(!params[1].value.is_null());
            assert_eq!(
                params[0].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_NULL
            );
            // rust-mysql sends all numbers as LONGLONG :'(
            assert_eq!(
                params[1].coltype,
                myc::constants::ColumnType::MYSQL_TYPE_LONGLONG
            );
            assert_eq!(
                std::convert::TryInto::<i8>::try_into(params[1].value)
                    .expect("Error calling try_into"),
                42i8
            );

            let cols = cols.clone();
            Box::pin(async move {
                let mut w = w.start(&cols).await?;
                w.write_row(vec![None::<i16>, Some(42)]).await?;
                w.finish().await
            })
        },
        |_, _| unreachable!(),
    )
    .with_params(params)
    .with_columns(cols2)
    .test(|db| {
        let res = db
            .exec::<Row, _, _>(
                "SELECT a, b FROM x WHERE c = ? AND d = ?",
                (mysql::Value::NULL, 42),
            )
            .unwrap();
        let row = res.first().unwrap();
        assert_eq!(row.as_ref(0), Some(&mysql::Value::NULL));
        assert_eq!(row.get::<i16, _>(1), Some(42));
    })
}

#[test]
fn prepared_no_rows() {
    let cols = vec![Column {
        table: String::new(),
        column: "a".to_owned(),
        coltype: myc::constants::ColumnType::MYSQL_TYPE_SHORT,
        column_length: None,
        colflags: myc::constants::ColumnFlags::empty(),
        character_set: DEFAULT_CHARACTER_SET,
    }];
    let cols2 = cols.clone();
    TestingShim::new(
        |_, _| unreachable!(),
        |_| 0,
        move |_, _, w| {
            let cols = cols.clone();
            Box::pin(async move { w.start(&cols[..]).await?.finish().await })
        },
        |_, _| unreachable!(),
    )
    .with_columns(cols2)
    .test(|db| {
        assert_eq!(
            db.exec::<Row, _, _>("SELECT a, b FROM foo", ())
                .unwrap()
                .len(),
            0
        );
    })
}

#[test]
fn prepared_no_cols_but_rows() {
    TestingShim::new(
        |_, _| unreachable!(),
        |_| 0,
        move |_, _, w| {
            Box::pin(async move {
                let mut w = w.start(&[]).await?;
                w.write_col(42)?;
                w.finish().await
            })
        },
        |_, _| unreachable!(),
    )
    .test(|db| {
        assert_eq!(
            db.exec::<Row, _, _>("SELECT a, b FROM foo", ())
                .unwrap()
                .len(),
            0
        );
    })
}

#[test]
fn prepared_no_cols() {
    TestingShim::new(
        |_, _| unreachable!(),
        |_| 0,
        move |_, _, w| Box::pin(async move { w.start(&[]).await?.finish().await }),
        |_, _| unreachable!(),
    )
    .test(|db| {
        assert_eq!(
            db.exec::<Row, _, _>("SELECT a, b FROM foo", ())
                .unwrap()
                .len(),
            0
        );
    })
}

#[test]
fn really_long_query() {
    let long = "CREATE TABLE `stories` (`id` int unsigned NOT NULL AUTO_INCREMENT PRIMARY KEY, `always_null` int, `created_at` datetime, `user_id` int unsigned, `url` varchar(250) DEFAULT '', `title` varchar(150) DEFAULT '' NOT NULL, `description` mediumtext, `short_id` varchar(6) DEFAULT '' NOT NULL, `is_expired` tinyint(1) DEFAULT 0 NOT NULL, `is_moderated` tinyint(1) DEFAULT 0 NOT NULL, `markeddown_description` mediumtext, `story_cache` mediumtext, `merged_story_id` int, `unavailable_at` datetime, `twitter_id` varchar(20), `user_is_author` tinyint(1) DEFAULT 0,  INDEX `index_stories_on_created_at`  (`created_at`), fulltext INDEX `index_stories_on_description`  (`description`),   INDEX `is_idxes`  (`is_expired`, `is_moderated`),  INDEX `index_stories_on_is_expired`  (`is_expired`),  INDEX `index_stories_on_is_moderated`  (`is_moderated`),  INDEX `index_stories_on_merged_story_id`  (`merged_story_id`), UNIQUE INDEX `unique_short_id`  (`short_id`), fulltext INDEX `index_stories_on_story_cache`  (`story_cache`), fulltext INDEX `index_stories_on_title`  (`title`),  INDEX `index_stories_on_twitter_id`  (`twitter_id`),  INDEX `url`  (`url`(191)),  INDEX `index_stories_on_user_id`  (`user_id`)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4;";
    TestingShim::new(
        move |q, w| {
            assert_eq!(q, long);
            Box::pin(async move { w.start(&[]).await?.finish().await })
        },
        |_| 0,
        |_, _, _| unreachable!(),
        |_, _| unreachable!(),
    )
    .test(move |db| {
        db.query::<Row, _>(long).unwrap();
    })
}
