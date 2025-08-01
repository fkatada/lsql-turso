//! VDBE bytecode generation for pragma statements.
//! More info: https://www.sqlite.org/pragma.html.

use chrono::Datelike;
use std::rc::Rc;
use std::sync::Arc;
use turso_sqlite3_parser::ast::{self, ColumnDefinition, Expr};
use turso_sqlite3_parser::ast::{PragmaName, QualifiedName};

use crate::pragma::pragma_for;
use crate::schema::Schema;
use crate::storage::pager::AutoVacuumMode;
use crate::storage::sqlite3_ondisk::{DatabaseEncoding, MIN_PAGE_CACHE_SIZE};
use crate::storage::wal::CheckpointMode;
use crate::translate::schema::translate_create_table;
use crate::util::{normalize_ident, parse_signed_number, parse_string};
use crate::vdbe::builder::{ProgramBuilder, ProgramBuilderOpts};
use crate::vdbe::insn::{Cookie, Insn};
use crate::{bail_parse_error, storage, CaptureDataChangesMode, LimboError, Value};
use std::str::FromStr;
use strum::IntoEnumIterator;

use super::integrity_check::translate_integrity_check;
use crate::storage::header_accessor;
use crate::storage::pager::Pager;
use crate::translate::emitter::TransactionMode;

fn list_pragmas(program: &mut ProgramBuilder) {
    for x in PragmaName::iter() {
        let register = program.emit_string8_new_reg(x.to_string());
        program.emit_result_row(register, 1);
    }
    program.add_pragma_result_column("pragma_list".into());
    program.epilogue(TransactionMode::None);
}

#[allow(clippy::too_many_arguments)]
pub fn translate_pragma(
    schema: &Schema,
    name: &ast::QualifiedName,
    body: Option<ast::PragmaBody>,
    pager: Rc<Pager>,
    connection: Arc<crate::Connection>,
    mut program: ProgramBuilder,
) -> crate::Result<ProgramBuilder> {
    let opts = ProgramBuilderOpts {
        num_cursors: 0,
        approx_num_insns: 20,
        approx_num_labels: 0,
    };
    program.extend(&opts);

    if name.name.as_str().eq_ignore_ascii_case("pragma_list") {
        list_pragmas(&mut program);
        return Ok(program);
    }

    let pragma = match PragmaName::from_str(name.name.as_str()) {
        Ok(pragma) => pragma,
        Err(_) => bail_parse_error!("Not a valid pragma name"),
    };

    let (mut program, mode) = match body {
        None => query_pragma(pragma, schema, None, pager, connection, program)?,
        Some(ast::PragmaBody::Equals(value) | ast::PragmaBody::Call(value)) => match pragma {
            PragmaName::TableInfo => {
                query_pragma(pragma, schema, Some(value), pager, connection, program)?
            }
            _ => update_pragma(pragma, schema, value, pager, connection, program)?,
        },
    };
    program.epilogue(mode);

    Ok(program)
}

fn update_pragma(
    pragma: PragmaName,
    schema: &Schema,
    value: ast::Expr,
    pager: Rc<Pager>,
    connection: Arc<crate::Connection>,
    mut program: ProgramBuilder,
) -> crate::Result<(ProgramBuilder, TransactionMode)> {
    match pragma {
        PragmaName::ApplicationId => {
            let data = parse_signed_number(&value)?;
            let app_id_value = match data {
                Value::Integer(i) => i as i32,
                Value::Float(f) => f as i32,
                _ => unreachable!(),
            };

            program.emit_insn(Insn::SetCookie {
                db: 0,
                cookie: Cookie::ApplicationId,
                value: app_id_value,
                p5: 1,
            });
            Ok((program, TransactionMode::Write))
        }
        PragmaName::CacheSize => {
            let cache_size = match parse_signed_number(&value)? {
                Value::Integer(size) => size,
                Value::Float(size) => size as i64,
                _ => bail_parse_error!("Invalid value for cache size pragma"),
            };
            update_cache_size(cache_size, pager, connection)?;
            Ok((program, TransactionMode::None))
        }
        PragmaName::Encoding => {
            let year = chrono::Local::now().year();
            bail_parse_error!("It's {year}. UTF-8 won.");
        }
        PragmaName::JournalMode => query_pragma(
            PragmaName::JournalMode,
            schema,
            None,
            pager,
            connection,
            program,
        ),
        PragmaName::LegacyFileFormat => Ok((program, TransactionMode::None)),
        PragmaName::WalCheckpoint => query_pragma(
            PragmaName::WalCheckpoint,
            schema,
            Some(value),
            pager,
            connection,
            program,
        ),
        PragmaName::PageCount => query_pragma(
            PragmaName::PageCount,
            schema,
            None,
            pager,
            connection,
            program,
        ),
        PragmaName::UserVersion => {
            let data = parse_signed_number(&value)?;
            let version_value = match data {
                Value::Integer(i) => i as i32,
                Value::Float(f) => f as i32,
                _ => unreachable!(),
            };

            program.emit_insn(Insn::SetCookie {
                db: 0,
                cookie: Cookie::UserVersion,
                value: version_value,
                p5: 1,
            });
            Ok((program, TransactionMode::Write))
        }
        PragmaName::SchemaVersion => {
            // SQLite allowing this to be set is an incredibly stupid idea in my view.
            // In "defensive mode", this is a silent nop. So let's emulate that always.
            program.emit_insn(Insn::Noop {});
            Ok((program, TransactionMode::None))
        }
        PragmaName::TableInfo => {
            // because we need control over the write parameter for the transaction,
            // this should be unreachable. We have to force-call query_pragma before
            // getting here
            unreachable!();
        }
        PragmaName::PageSize => {
            let page_size = match parse_signed_number(&value)? {
                Value::Integer(size) => size,
                Value::Float(size) => size as i64,
                _ => bail_parse_error!("Invalid value for page size pragma"),
            };
            update_page_size(connection, page_size as u32)?;
            Ok((program, TransactionMode::None))
        }
        PragmaName::AutoVacuum => {
            let auto_vacuum_mode = match value {
                Expr::Name(name) => {
                    let name = name.as_str().to_lowercase();
                    match name.as_str() {
                        "none" => 0,
                        "full" => 1,
                        "incremental" => 2,
                        _ => {
                            return Err(LimboError::InvalidArgument(
                                "invalid auto vacuum mode".to_string(),
                            ));
                        }
                    }
                }
                _ => {
                    return Err(LimboError::InvalidArgument(
                        "invalid auto vacuum mode".to_string(),
                    ))
                }
            };
            match auto_vacuum_mode {
                0 => update_auto_vacuum_mode(AutoVacuumMode::None, 0, pager)?,
                1 => update_auto_vacuum_mode(AutoVacuumMode::Full, 1, pager)?,
                2 => update_auto_vacuum_mode(AutoVacuumMode::Incremental, 1, pager)?,
                _ => {
                    return Err(LimboError::InvalidArgument(
                        "invalid auto vacuum mode".to_string(),
                    ))
                }
            }
            let largest_root_page_number_reg = program.alloc_register();
            program.emit_insn(Insn::ReadCookie {
                db: 0,
                dest: largest_root_page_number_reg,
                cookie: Cookie::LargestRootPageNumber,
            });
            let set_cookie_label = program.allocate_label();
            program.emit_insn(Insn::If {
                reg: largest_root_page_number_reg,
                target_pc: set_cookie_label,
                jump_if_null: false,
            });
            program.emit_insn(Insn::Halt {
                err_code: 0,
                description: "Early halt because auto vacuum mode is not enabled".to_string(),
            });
            program.resolve_label(set_cookie_label, program.offset());
            program.emit_insn(Insn::SetCookie {
                db: 0,
                cookie: Cookie::IncrementalVacuum,
                value: auto_vacuum_mode - 1,
                p5: 0,
            });
            Ok((program, TransactionMode::None))
        }
        PragmaName::IntegrityCheck => unreachable!("integrity_check cannot be set"),
        PragmaName::UnstableCaptureDataChangesConn => {
            let value = parse_string(&value)?;
            // todo(sivukhin): ideally, we should consistently update capture_data_changes connection flag only after successfull execution of schema change statement
            // but for now, let's keep it as is...
            let opts = CaptureDataChangesMode::parse(&value)?;
            if let Some(table) = &opts.table() {
                // make sure that we have table created
                program = translate_create_table(
                    QualifiedName::single(ast::Name::from_str(table)),
                    false,
                    ast::CreateTableBody::columns_and_constraints_from_definition(
                        turso_cdc_table_columns(),
                        None,
                        ast::TableOptions::NONE,
                    )
                    .unwrap(),
                    true,
                    schema,
                    program,
                )?;
            }
            connection.set_capture_data_changes(opts);
            Ok((program, TransactionMode::Write))
        }
        PragmaName::DatabaseList => unreachable!("database_list cannot be set"),
    }
}

fn query_pragma(
    pragma: PragmaName,
    schema: &Schema,
    value: Option<ast::Expr>,
    pager: Rc<Pager>,
    connection: Arc<crate::Connection>,
    mut program: ProgramBuilder,
) -> crate::Result<(ProgramBuilder, TransactionMode)> {
    let register = program.alloc_register();
    match pragma {
        PragmaName::ApplicationId => {
            program.emit_insn(Insn::ReadCookie {
                db: 0,
                dest: register,
                cookie: Cookie::ApplicationId,
            });
            program.add_pragma_result_column(pragma.to_string());
            program.emit_result_row(register, 1);
            Ok((program, TransactionMode::Read))
        }
        PragmaName::CacheSize => {
            program.emit_int(connection.get_cache_size() as i64, register);
            program.emit_result_row(register, 1);
            program.add_pragma_result_column(pragma.to_string());
            Ok((program, TransactionMode::None))
        }
        PragmaName::DatabaseList => {
            let base_reg = register;
            program.alloc_registers(2);

            // Get all databases (main + attached) and emit a row for each
            let all_databases = connection.list_all_databases();
            for (seq_number, name, file_path) in all_databases {
                // seq (sequence number)
                program.emit_int(seq_number as i64, base_reg);

                // name (alias)
                program.emit_string8(name, base_reg + 1);

                // file path
                program.emit_string8(file_path, base_reg + 2);

                program.emit_result_row(base_reg, 3);
            }

            let pragma = pragma_for(&pragma);
            for col_name in pragma.columns.iter() {
                program.add_pragma_result_column(col_name.to_string());
            }
            Ok((program, TransactionMode::None))
        }
        PragmaName::Encoding => {
            let encoding: &str = if !pager.db_state.is_initialized() {
                DatabaseEncoding::Utf8
            } else {
                let encoding: DatabaseEncoding =
                    header_accessor::get_text_encoding(&pager)?.try_into()?;
                encoding
            }
            .into();
            program.emit_string8(encoding.into(), register);
            program.emit_result_row(register, 1);
            program.add_pragma_result_column(pragma.to_string());
            Ok((program, TransactionMode::None))
        }
        PragmaName::JournalMode => {
            program.emit_string8("wal".into(), register);
            program.emit_result_row(register, 1);
            program.add_pragma_result_column(pragma.to_string());
            Ok((program, TransactionMode::None))
        }
        PragmaName::LegacyFileFormat => Ok((program, TransactionMode::None)),
        PragmaName::WalCheckpoint => {
            // Checkpoint uses 3 registers: P1, P2, P3. Ref Insn::Checkpoint for more info.
            // Allocate two more here as one was allocated at the top.
            let mode = match value {
                Some(ast::Expr::Name(name)) => {
                    let mode_name = normalize_ident(name.as_str());
                    CheckpointMode::from_str(&mode_name).map_err(|e| {
                        LimboError::ParseError(format!("Unknown Checkpoint Mode: {e}"))
                    })?
                }
                _ => CheckpointMode::Passive,
            };

            program.alloc_registers(2);
            program.emit_insn(Insn::Checkpoint {
                database: 0,
                checkpoint_mode: mode,
                dest: register,
            });
            program.emit_result_row(register, 3);
            Ok((program, TransactionMode::None))
        }
        PragmaName::PageCount => {
            program.emit_insn(Insn::PageCount {
                db: 0,
                dest: register,
            });
            program.emit_result_row(register, 1);
            program.add_pragma_result_column(pragma.to_string());
            Ok((program, TransactionMode::Read))
        }
        PragmaName::TableInfo => {
            let table = match value {
                Some(ast::Expr::Name(name)) => {
                    let tbl = normalize_ident(name.as_str());
                    schema.get_table(&tbl)
                }
                _ => None,
            };

            let base_reg = register;
            program.alloc_registers(5);
            if let Some(table) = table {
                // According to the SQLite documentation: "The 'cid' column should not be taken to
                // mean more than 'rank within the current result set'."
                // Therefore, we enumerate only after filtering out hidden columns.
                for (i, column) in table.columns().iter().filter(|col| !col.hidden).enumerate() {
                    // cid
                    program.emit_int(i as i64, base_reg);
                    // name
                    program.emit_string8(column.name.clone().unwrap_or_default(), base_reg + 1);

                    // type
                    program.emit_string8(column.ty_str.clone(), base_reg + 2);

                    // notnull
                    program.emit_bool(column.notnull, base_reg + 3);

                    // dflt_value
                    match &column.default {
                        None => {
                            program.emit_null(base_reg + 4, None);
                        }
                        Some(expr) => {
                            program.emit_string8(expr.to_string(), base_reg + 4);
                        }
                    }

                    // pk
                    program.emit_bool(column.primary_key, base_reg + 5);

                    program.emit_result_row(base_reg, 6);
                }
            }
            let col_names = ["cid", "name", "type", "notnull", "dflt_value", "pk"];
            for name in col_names {
                program.add_pragma_result_column(name.into());
            }
            Ok((program, TransactionMode::None))
        }
        PragmaName::UserVersion => {
            program.emit_insn(Insn::ReadCookie {
                db: 0,
                dest: register,
                cookie: Cookie::UserVersion,
            });
            program.add_pragma_result_column(pragma.to_string());
            program.emit_result_row(register, 1);
            Ok((program, TransactionMode::Read))
        }
        PragmaName::SchemaVersion => {
            program.emit_insn(Insn::ReadCookie {
                db: 0,
                dest: register,
                cookie: Cookie::SchemaVersion,
            });
            program.add_pragma_result_column(pragma.to_string());
            program.emit_result_row(register, 1);
            Ok((program, TransactionMode::Read))
        }
        PragmaName::PageSize => {
            program.emit_int(
                header_accessor::get_page_size(&pager).unwrap_or(connection.get_page_size()) as i64,
                register,
            );
            program.emit_result_row(register, 1);
            program.add_pragma_result_column(pragma.to_string());
            Ok((program, TransactionMode::None))
        }
        PragmaName::AutoVacuum => {
            let auto_vacuum_mode = pager.get_auto_vacuum_mode();
            let auto_vacuum_mode_i64: i64 = match auto_vacuum_mode {
                AutoVacuumMode::None => 0,
                AutoVacuumMode::Full => 1,
                AutoVacuumMode::Incremental => 2,
            };
            let register = program.alloc_register();
            program.emit_insn(Insn::Int64 {
                _p1: 0,
                out_reg: register,
                _p3: 0,
                value: auto_vacuum_mode_i64,
            });
            program.emit_result_row(register, 1);
            Ok((program, TransactionMode::None))
        }
        PragmaName::IntegrityCheck => {
            translate_integrity_check(schema, &mut program)?;
            Ok((program, TransactionMode::Read))
        }
        PragmaName::UnstableCaptureDataChangesConn => {
            let pragma = pragma_for(&pragma);
            let second_column = program.alloc_register();
            let opts = connection.get_capture_data_changes();
            program.emit_string8(opts.mode_name().to_string(), register);
            if let Some(table) = &opts.table() {
                program.emit_string8(table.to_string(), second_column);
            } else {
                program.emit_null(second_column, None);
            }
            program.emit_result_row(register, 2);
            program.add_pragma_result_column(pragma.columns[0].to_string());
            program.add_pragma_result_column(pragma.columns[1].to_string());
            Ok((program, TransactionMode::Read))
        }
    }
}

fn update_auto_vacuum_mode(
    auto_vacuum_mode: AutoVacuumMode,
    largest_root_page_number: u32,
    pager: Rc<Pager>,
) -> crate::Result<()> {
    header_accessor::set_vacuum_mode_largest_root_page(&pager, largest_root_page_number)?;
    pager.set_auto_vacuum_mode(auto_vacuum_mode);
    Ok(())
}

fn update_cache_size(
    value: i64,
    pager: Rc<Pager>,
    connection: Arc<crate::Connection>,
) -> crate::Result<()> {
    let mut cache_size_unformatted: i64 = value;

    let mut cache_size = if cache_size_unformatted < 0 {
        let kb = cache_size_unformatted.abs().saturating_mul(1024);
        let page_size = header_accessor::get_page_size(&pager)
            .unwrap_or(storage::sqlite3_ondisk::DEFAULT_PAGE_SIZE) as i64;
        if page_size == 0 {
            return Err(LimboError::InternalError(
                "Page size cannot be zero".to_string(),
            ));
        }
        kb / page_size
    } else {
        value
    };

    // SQLite uses this value as threshold for maximum cache size
    const MAX_SAFE_CACHE_SIZE: i64 = 2147450880;

    if cache_size > MAX_SAFE_CACHE_SIZE {
        cache_size = 0;
        cache_size_unformatted = 0;
    }

    if cache_size < 0 {
        cache_size = 0;
        cache_size_unformatted = 0;
    }

    let cache_size_usize = cache_size as usize;

    let final_cache_size = if cache_size_usize < MIN_PAGE_CACHE_SIZE {
        cache_size_unformatted = MIN_PAGE_CACHE_SIZE as i64;
        MIN_PAGE_CACHE_SIZE
    } else {
        cache_size_usize
    };

    connection.set_cache_size(cache_size_unformatted as i32);

    pager
        .change_page_cache_size(final_cache_size)
        .map_err(|e| LimboError::InternalError(format!("Failed to update page cache size: {e}")))?;

    Ok(())
}

pub const TURSO_CDC_DEFAULT_TABLE_NAME: &str = "turso_cdc";
fn turso_cdc_table_columns() -> Vec<ColumnDefinition> {
    vec![
        ast::ColumnDefinition {
            col_name: ast::Name::from_str("change_id"),
            col_type: Some(ast::Type {
                name: "INTEGER".to_string(),
                size: None,
            }),
            constraints: vec![ast::NamedColumnConstraint {
                name: None,
                constraint: ast::ColumnConstraint::PrimaryKey {
                    order: None,
                    conflict_clause: None,
                    auto_increment: true,
                },
            }],
        },
        ast::ColumnDefinition {
            col_name: ast::Name::from_str("change_time"),
            col_type: Some(ast::Type {
                name: "INTEGER".to_string(),
                size: None,
            }),
            constraints: vec![],
        },
        ast::ColumnDefinition {
            col_name: ast::Name::from_str("change_type"),
            col_type: Some(ast::Type {
                name: "INTEGER".to_string(),
                size: None,
            }),
            constraints: vec![],
        },
        ast::ColumnDefinition {
            col_name: ast::Name::from_str("table_name"),
            col_type: Some(ast::Type {
                name: "TEXT".to_string(),
                size: None,
            }),
            constraints: vec![],
        },
        ast::ColumnDefinition {
            col_name: ast::Name::from_str("id"),
            col_type: None,
            constraints: vec![],
        },
        ast::ColumnDefinition {
            col_name: ast::Name::from_str("before"),
            col_type: Some(ast::Type {
                name: "BLOB".to_string(),
                size: None,
            }),
            constraints: vec![],
        },
        ast::ColumnDefinition {
            col_name: ast::Name::from_str("after"),
            col_type: Some(ast::Type {
                name: "BLOB".to_string(),
                size: None,
            }),
            constraints: vec![],
        },
    ]
}

fn update_page_size(connection: Arc<crate::Connection>, page_size: u32) -> crate::Result<()> {
    connection.reset_page_size(page_size)?;
    Ok(())
}
