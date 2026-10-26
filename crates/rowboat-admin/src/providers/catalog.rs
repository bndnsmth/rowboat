//! Server catalog SQL. All selection goes through the optional-provider registry.
use super::{Operation, ServerKind, server};
use crate::objects::ObjectKind;
use anyhow::Result;
use rowboat_core::Engine;

pub(crate) fn object_sql(engine: Engine, kind: ObjectKind) -> Result<String> {
    super::operation_support(engine, Operation::ObjectCatalog(kind))?;
    use ObjectKind::*;
    Ok(match (server(engine, Operation::ObjectCatalog(kind))?, kind) {
        (ServerKind::Postgres, Function | Procedure) => format!(
            "SELECT n.nspname AS schema_name, p.proname AS object_name, pg_get_functiondef(p.oid) AS definition, pg_get_userbyid(p.proowner) AS owner, pg_get_function_identity_arguments(p.oid) AS details FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE p.prokind='{}' AND n.nspname NOT IN ('pg_catalog','information_schema') ORDER BY 1,2,5", if kind == Procedure {"p"} else {"f"}),
        (ServerKind::Postgres, Trigger) => "SELECT n.nspname AS schema_name, t.tgname AS object_name, pg_get_triggerdef(t.oid, true) AS definition, pg_get_userbyid(c.relowner) AS owner, c.relname AS details FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE NOT t.tgisinternal ORDER BY 1,2,5".into(),
        (ServerKind::Postgres, View | MaterializedView) => format!("SELECT n.nspname AS schema_name, c.relname AS object_name, pg_get_viewdef(c.oid,true) AS definition, pg_get_userbyid(c.relowner) AS owner, c.relkind::text AS details FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind='{}' AND n.nspname NOT IN ('pg_catalog','information_schema') ORDER BY 1,2", if kind == View {"v"} else {"m"}),
        (ServerKind::Postgres, Role) => "SELECT NULL::text AS schema_name, rolname AS object_name, NULL::text AS definition, NULL::text AS owner, json_build_object('login',rolcanlogin,'superuser',rolsuper,'create_db',rolcreatedb,'create_role',rolcreaterole,'inherit',rolinherit,'replication',rolreplication,'connection_limit',rolconnlimit,'valid_until',rolvaliduntil)::text AS details FROM pg_roles ORDER BY rolname".into(),
        (ServerKind::Postgres, Sequence) => "SELECT schemaname AS schema_name, sequencename AS object_name, NULL::text AS definition, sequenceowner AS owner, json_build_object('type',data_type,'start',start_value,'min',min_value,'max',max_value,'increment',increment_by,'cycle',cycle,'cache',cache_size,'last',last_value)::text AS details FROM pg_sequences ORDER BY 1,2".into(),
        (ServerKind::MySql, Function | Procedure) => format!("SELECT ROUTINE_SCHEMA AS schema_name, ROUTINE_NAME AS object_name, ROUTINE_DEFINITION AS definition, DEFINER AS owner, CONCAT('SQL SECURITY ',SECURITY_TYPE,'; ',SQL_DATA_ACCESS) AS details FROM information_schema.ROUTINES WHERE ROUTINE_TYPE='{}' ORDER BY 1,2",if kind == Procedure {"PROCEDURE"} else {"FUNCTION"}),
        (ServerKind::MySql, Trigger) => "SELECT TRIGGER_SCHEMA AS schema_name, TRIGGER_NAME AS object_name, ACTION_STATEMENT AS definition, DEFINER AS owner, CONCAT(ACTION_TIMING,' ',EVENT_MANIPULATION,' ON ',EVENT_OBJECT_TABLE) AS details FROM information_schema.TRIGGERS ORDER BY 1,2".into(),
        (ServerKind::MySql, View) => "SELECT TABLE_SCHEMA AS schema_name, TABLE_NAME AS object_name, VIEW_DEFINITION AS definition, DEFINER AS owner, CONCAT('SQL SECURITY ',SECURITY_TYPE,'; CHECK ',CHECK_OPTION) AS details FROM information_schema.VIEWS WHERE TABLE_SCHEMA NOT IN ('sys','mysql','information_schema','performance_schema') ORDER BY 1,2".into(),
        (ServerKind::MySql, Role) => "SELECT Host AS schema_name, User AS object_name, NULL AS definition, NULL AS owner, CONCAT('plugin=',plugin,'; locked=',account_locked) AS details FROM mysql.user ORDER BY User,Host".into(),
        (ServerKind::MySql, MaterializedView | Sequence) => return Err(super::UnsupportedOperation {
            engine, operation: Operation::ObjectCatalog(kind),
            reason: "The provider does not support this object kind",
        }.into()),
    })
}

pub(crate) fn connection_info_sql(engine: Engine) -> Result<&'static str> {
    Ok(match server(engine, Operation::ConnectionInfo)? {
        ServerKind::Postgres => {
            "SELECT current_database() AS database, current_user AS current_user, session_user AS session_user, version() AS version, inet_server_addr()::text AS server_address, inet_server_port() AS server_port, pg_backend_pid() AS connection_id, current_setting('TimeZone') AS timezone, current_setting('transaction_read_only') AS read_only"
        }
        ServerKind::MySql => {
            "SELECT DATABASE() AS current_database, CURRENT_USER() AS authenticated_user, USER() AS session_user, VERSION() AS version, @@hostname AS server_host, @@port AS server_port, CONNECTION_ID() AS connection_id, @@session.time_zone AS time_zone, @@session.transaction_read_only AS read_only"
        }
    })
}

pub(crate) fn server_activity_sql(engine: Engine) -> Result<&'static str> {
    Ok(match server(engine, Operation::ServerActivity)? {
        ServerKind::Postgres => {
            "SELECT pid, datname, usename, application_name, client_addr::text, backend_start, xact_start, query_start, state, wait_event_type, wait_event, query FROM pg_stat_activity ORDER BY query_start NULLS LAST"
        }
        ServerKind::MySql => {
            "SELECT ID, USER, HOST, DB, COMMAND, TIME, STATE, INFO FROM information_schema.PROCESSLIST ORDER BY TIME DESC"
        }
    })
}

pub(crate) fn namespaces_sql(engine: Engine) -> Result<&'static str> {
    Ok(match server(engine, Operation::Namespaces)? {
        ServerKind::Postgres => {
            "SELECT d.datname AS database_name, pg_get_userbyid(d.datdba) AS owner, pg_encoding_to_char(d.encoding) AS encoding, d.datcollate AS collation FROM pg_database d WHERE NOT d.datistemplate ORDER BY d.datname"
        }
        ServerKind::MySql => {
            "SELECT SCHEMA_NAME AS database_name, DEFAULT_CHARACTER_SET_NAME AS encoding, DEFAULT_COLLATION_NAME AS collation FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME"
        }
    })
}

pub(crate) fn schemas_sql(engine: Engine) -> Result<&'static str> {
    match server(engine, Operation::Namespaces)? {
        ServerKind::Postgres => Ok(
            "SELECT schema_name, schema_owner FROM information_schema.schemata ORDER BY schema_name",
        ),
        ServerKind::MySql => namespaces_sql(engine),
    }
}

pub(crate) fn connection_origin_sql(engine: Engine) -> Result<&'static str> {
    Ok(match server(engine, Operation::ConnectionInfo)? {
        ServerKind::Postgres => {
            "SELECT json_build_object('database',current_database(),'database_oid',(SELECT oid::text FROM pg_database WHERE datname=current_database()),'session',pg_backend_pid(),'user',current_user,'address',inet_server_addr()::text,'port',inet_server_port(),'started',pg_postmaster_start_time()::text,'version',current_setting('server_version'),'encoding',current_setting('client_encoding'),'standard_conforming_strings',current_setting('standard_conforming_strings'),'search_path',current_setting('search_path'))::text"
        }
        ServerKind::MySql => {
            "SELECT JSON_OBJECT('database',DATABASE(),'session',CONNECTION_ID(),'user',CURRENT_USER(),'uuid',@@server_uuid,'host',@@hostname,'port',@@port,'version',VERSION(),'sql_mode',@@session.sql_mode,'encoding',@@character_set_connection,'client_encoding',@@character_set_client,'collation',@@collation_connection)"
        }
    })
}
