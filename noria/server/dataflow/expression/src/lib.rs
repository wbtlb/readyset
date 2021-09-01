use std::borrow::Cow;
use std::cmp::min;
use std::convert::TryFrom;
use std::fmt;
use std::fmt::Formatter;
use std::ops::{Add, Sub};
use std::sync::Arc;

use chrono::{Datelike, LocalResult, NaiveDate, NaiveDateTime, TimeZone};
use chrono_tz::Tz;
use maths::int::integer_rnd;
use mysql_time::MysqlTime;
use nom_sql::{BinaryOperator, SqlType};
use noria::util::like::{CaseInsensitive, CaseSensitive, LikePattern};
use noria::{DataType, ReadySetError, ReadySetResult};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BuiltinFunction {
    /// convert_tz(expr, expr, expr)
    ConvertTZ(Box<Expression>, Box<Expression>, Box<Expression>),
    /// dayofweek(expr)
    DayOfWeek(Box<Expression>),
    /// ifnull(expr, expr)
    IfNull(Box<Expression>, Box<Expression>),
    /// month(expr)
    Month(Box<Expression>),
    /// timediff(expr, expr)
    Timediff(Box<Expression>, Box<Expression>),
    /// addtime(expr, expr)
    Addtime(Box<Expression>, Box<Expression>),
    /// round(expr, prec)
    Round(Box<Expression>, Box<Expression>),
}

impl BuiltinFunction {
    pub fn from_name_and_args<A>(name: &str, args: A) -> Result<Self, ReadySetError>
    where
        A: IntoIterator<Item = Expression>,
    {
        let mut args = args.into_iter();
        match name {
            "convert_tz" => {
                let arity_error = || ReadySetError::ArityError("convert_tz".to_owned());
                Ok(Self::ConvertTZ(
                    Box::new(args.next().ok_or_else(arity_error)?),
                    Box::new(args.next().ok_or_else(arity_error)?),
                    Box::new(args.next().ok_or_else(arity_error)?),
                ))
            }
            "dayofweek" => {
                let arity_error = || ReadySetError::ArityError("dayofweek".to_owned());
                Ok(Self::DayOfWeek(Box::new(
                    args.next().ok_or_else(arity_error)?,
                )))
            }
            "ifnull" => {
                let arity_error = || ReadySetError::ArityError("ifnull".to_owned());
                Ok(Self::IfNull(
                    Box::new(args.next().ok_or_else(arity_error)?),
                    Box::new(args.next().ok_or_else(arity_error)?),
                ))
            }
            "month" => {
                let arity_error = || ReadySetError::ArityError("month".to_owned());
                Ok(Self::Month(Box::new(args.next().ok_or_else(arity_error)?)))
            }
            "timediff" => {
                let arity_error = || ReadySetError::ArityError("timediff".to_owned());
                Ok(Self::Timediff(
                    Box::new(args.next().ok_or_else(arity_error)?),
                    Box::new(args.next().ok_or_else(arity_error)?),
                ))
            }
            "addtime" => {
                let arity_error = || ReadySetError::ArityError("addtime".to_owned());
                Ok(Self::Addtime(
                    Box::new(args.next().ok_or_else(arity_error)?),
                    Box::new(args.next().ok_or_else(arity_error)?),
                ))
            }
            "round" => {
                let arity_error = || ReadySetError::ArityError("round".to_owned());
                Ok(Self::Round(
                    Box::new(args.next().ok_or_else(arity_error)?),
                    Box::new(args.next().unwrap_or(Expression::Literal(DataType::Int(0)))),
                ))
            }
            _ => Err(ReadySetError::NoSuchFunction(name.to_owned())),
        }
    }
}

impl fmt::Display for BuiltinFunction {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use BuiltinFunction::*;

        match self {
            ConvertTZ(arg1, arg2, arg3) => {
                write!(f, "convert_tz({},{},{})", arg1, arg2, arg3)
            }
            DayOfWeek(arg) => {
                write!(f, "dayofweek({})", arg)
            }
            IfNull(arg1, arg2) => {
                write!(f, "ifnull({}, {})", arg1, arg2)
            }
            Month(arg) => {
                write!(f, "month({})", arg)
            }
            Timediff(arg1, arg2) => {
                write!(f, "timediff({}, {})", arg1, arg2)
            }
            Addtime(arg1, arg2) => {
                write!(f, "addtime({}, {})", arg1, arg2)
            }
            Round(arg1, precision) => {
                write!(f, "round({}, {})", arg1, precision)
            }
        }
    }
}

/// Expressions that can be evaluated during execution of a query
///
/// This type, which is the final lowered version of the original Expression AST, essentially
/// represents a desugared version of [`nom_sql::Expression`], with the following transformations
/// applied during lowering:
///
/// - Literals replaced with their corresponding [`DataType`]
/// - [Column references](nom_sql::Column) resolved into column indices in the parent node.
/// - Function calls resolved, and arities checked
/// - Desugaring x IN (y, z, ...) to `x = y OR x = z OR ...`
///   and x NOT IN (y, z, ...) to `x != y AND x = z AND ...`
///
/// During forward processing of dataflow, instances of these expressions are
/// [evaluated](Expression::eval) by both projection nodes and filter nodes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Expression {
    /// A reference to a column, by index, in the parent node
    Column(usize),

    /// A literal DataType value
    Literal(DataType),

    /// A binary operation
    Op {
        op: BinaryOperator,
        left: Box<Expression>,
        right: Box<Expression>,
    },

    /// CAST(expr AS type)
    Cast(Box<Expression>, SqlType),

    Call(BuiltinFunction),

    CaseWhen {
        condition: Box<Expression>,
        then_expr: Box<Expression>,
        else_expr: Box<Expression>,
    },
}

impl fmt::Display for Expression {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use Expression::*;

        match self {
            Column(u) => write!(f, "{}", u),
            Literal(l) => write!(f, "(lit: {})", l),
            Op { op, left, right } => write!(f, "({} {} {})", left, op, right),
            Cast(expr, ty) => write!(f, "cast({} as {})", expr, ty),
            Call(func) => write!(f, "{}", func),
            CaseWhen {
                condition,
                then_expr,
                else_expr,
            } => write!(
                f,
                "case when {} then {} else {}",
                condition, then_expr, else_expr
            ),
        }
    }
}

macro_rules! try_cast_or_none {
    ($datatype:expr, $sqltype:expr) => {
        match $datatype.coerce_to($sqltype) {
            Ok(v) => v,
            Err(_) => return Ok(Cow::Owned(DataType::None)),
        };
    };
}

macro_rules! get_time_or_default {
    ($datatype:expr) => {
        $datatype
            .coerce_to(&SqlType::Timestamp)
            .or($datatype.coerce_to(&SqlType::Time))
            .unwrap_or(Cow::Owned(DataType::None));
    };
}

macro_rules! non_null {
    ($datatype:expr) => {
        if let Some(dt) = $datatype.non_null() {
            dt
        } else {
            return Ok(Cow::Owned(DataType::None));
        }
    };
}

impl Expression {
    /// Evaluate this expression, given a source record to pull columns from
    pub fn eval<'a>(&self, record: &'a [DataType]) -> ReadySetResult<Cow<'a, DataType>> {
        use Expression::*;

        match self {
            Column(c) => record
                .get(*c)
                .map(Cow::Borrowed)
                .ok_or(ReadySetError::ProjectExpressionInvalidColumnIndex(*c)),
            Literal(dt) => Ok(Cow::Owned(dt.clone())),
            Op { op, left, right } => {
                use BinaryOperator::*;

                let left = left.eval(record)?;
                let right = right.eval(record)?;

                macro_rules! like {
                    ($case_sensitivity: expr, $negated: expr) => {{
                        match (
                            left.coerce_to(&SqlType::Text),
                            right.coerce_to(&SqlType::Text),
                        ) {
                            (Ok(left), Ok(right)) => {
                                // NOTE(grfn): At some point, we may want to optimize this to
                                // pre-cache the LikePattern if the value is constant, since
                                // constructing a new LikePattern can be kinda slow
                                let pat = LikePattern::new(
                                    // unwrap: we just coerced it to Text, so it's definitely a string
                                    String::try_from(right.as_ref()).unwrap().as_str(),
                                    $case_sensitivity,
                                );
                                let matches =
                                    // unwrap: we just coerced it to Text, so it's definitely a string
                                    pat.matches(String::try_from(left.as_ref()).unwrap().as_str());
                                Ok(Cow::Owned(if $negated { !matches } else { matches }.into()))
                            }
                            // Anything that isn't Text or text-coercible can never be LIKE
                            // anything, so we return true if not negated, false otherwise
                            _ => Ok(Cow::Owned(DataType::from(!$negated))),
                        }
                    }};
                }

                match op {
                    Add => Ok(Cow::Owned((non_null!(left) + non_null!(right))?)),
                    Subtract => Ok(Cow::Owned((non_null!(left) - non_null!(right))?)),
                    Multiply => Ok(Cow::Owned((non_null!(left) * non_null!(right))?)),
                    Divide => Ok(Cow::Owned((non_null!(left) / non_null!(right))?)),
                    And => Ok(Cow::Owned(
                        (non_null!(left).is_truthy() && non_null!(right).is_truthy()).into(),
                    )),
                    Or => Ok(Cow::Owned(
                        (non_null!(left).is_truthy() || non_null!(right).is_truthy()).into(),
                    )),
                    Equal => Ok(Cow::Owned((non_null!(left) == non_null!(right)).into())),
                    NotEqual => Ok(Cow::Owned((non_null!(left) != non_null!(right)).into())),
                    Greater => Ok(Cow::Owned((non_null!(left) > non_null!(right)).into())),
                    GreaterOrEqual => Ok(Cow::Owned((non_null!(left) >= non_null!(right)).into())),
                    Less => Ok(Cow::Owned((non_null!(left) < non_null!(right)).into())),
                    LessOrEqual => Ok(Cow::Owned((non_null!(left) <= non_null!(right)).into())),
                    Is => Ok(Cow::Owned((left == right).into())),
                    IsNot => Ok(Cow::Owned((left != right).into())),
                    Like => like!(CaseSensitive, false),
                    NotLike => like!(CaseSensitive, true),
                    ILike => like!(CaseInsensitive, false),
                    NotILike => like!(CaseInsensitive, true),
                }
            }
            Cast(expr, ty) => match expr.eval(record)? {
                Cow::Borrowed(val) => Ok(val.coerce_to(ty)?),
                Cow::Owned(val) => Ok(Cow::Owned(non_null!(val).coerce_to(ty)?.into_owned())),
            },
            Call(func) => match func {
                BuiltinFunction::ConvertTZ(arg1, arg2, arg3) => {
                    let param1 = arg1.eval(record)?;
                    let param2 = arg2.eval(record)?;
                    let param3 = arg3.eval(record)?;
                    let param1_cast = try_cast_or_none!(param1, &SqlType::Timestamp);
                    let param2_cast = try_cast_or_none!(param2, &SqlType::Text);
                    let param3_cast = try_cast_or_none!(param3, &SqlType::Text);
                    match convert_tz(
                        &(NaiveDateTime::try_from(param1_cast.as_ref()))?,
                        <&str>::try_from(param2_cast.as_ref())?,
                        <&str>::try_from(param3_cast.as_ref())?,
                    ) {
                        Ok(v) => Ok(Cow::Owned(DataType::Timestamp(v))),
                        Err(_) => Ok(Cow::Owned(DataType::None)),
                    }
                }
                BuiltinFunction::DayOfWeek(arg) => {
                    let param = arg.eval(record)?;
                    let param_cast = try_cast_or_none!(param, &SqlType::Date);
                    Ok(Cow::Owned(DataType::Int(day_of_week(
                        &(NaiveDate::try_from(param_cast.as_ref())?),
                    ) as i32)))
                }
                BuiltinFunction::IfNull(arg1, arg2) => {
                    let param1 = arg1.eval(record)?;
                    let param2 = arg2.eval(record)?;
                    if param1.is_none() {
                        Ok(param2)
                    } else {
                        Ok(param1)
                    }
                }
                BuiltinFunction::Month(arg) => {
                    let param = arg.eval(record)?;
                    let param_cast = try_cast_or_none!(param, &SqlType::Date);
                    Ok(Cow::Owned(DataType::UnsignedInt(month(
                        &(NaiveDate::try_from(non_null!(param_cast))?),
                    )
                        as u32)))
                }
                BuiltinFunction::Timediff(arg1, arg2) => {
                    let param1 = arg1.eval(record)?;
                    let param2 = arg2.eval(record)?;
                    let null_result = Ok(Cow::Owned(DataType::None));
                    let time_param1 = get_time_or_default!(param1);
                    let time_param2 = get_time_or_default!(param2);
                    if time_param1.is_none()
                        || time_param1
                            .sql_type()
                            .and_then(|st| time_param2.sql_type().map(|st2| (st, st2)))
                            .filter(|(st1, st2)| st1.eq(st2))
                            .is_none()
                    {
                        return null_result;
                    }
                    let time = if time_param1.is_datetime() {
                        timediff_datetimes(
                            &(NaiveDateTime::try_from(time_param1.as_ref())?),
                            &(NaiveDateTime::try_from(time_param2.as_ref())?),
                        )
                    } else {
                        timediff_times(
                            &(MysqlTime::try_from(time_param1.as_ref())?),
                            &(MysqlTime::try_from(time_param2.as_ref())?),
                        )
                    };
                    Ok(Cow::Owned(DataType::Time(Arc::new(time))))
                }
                BuiltinFunction::Addtime(arg1, arg2) => {
                    let param1 = arg1.eval(record)?;
                    let param2 = arg2.eval(record)?;
                    let time_param2 = get_time_or_default!(param2);
                    if time_param2.is_datetime() {
                        return Ok(Cow::Owned(DataType::None));
                    }
                    let time_param1 = get_time_or_default!(param1);
                    if time_param1.is_datetime() {
                        Ok(Cow::Owned(DataType::Timestamp(addtime_datetime(
                            &(NaiveDateTime::try_from(time_param1.as_ref())?),
                            &(MysqlTime::try_from(time_param2.as_ref())?),
                        ))))
                    } else {
                        Ok(Cow::Owned(DataType::Time(Arc::new(addtime_times(
                            &(MysqlTime::try_from(time_param1.as_ref())?),
                            &(MysqlTime::try_from(time_param2.as_ref())?),
                        )))))
                    }
                }
                BuiltinFunction::Round(arg1, arg2) => {
                    let expr = arg1.eval(record)?;
                    let param2 = arg2.eval(record)?;
                    let rnd_prec = match non_null!(param2) {
                        DataType::Int(inner) => *inner as i32,
                        DataType::UnsignedInt(inner) => *inner as i32,
                        DataType::BigInt(inner) => *inner as i32,
                        DataType::UnsignedBigInt(inner) => *inner as i32,
                        DataType::Real(f, _) => f.round() as i32,
                        _ => 0,
                    };

                    match non_null!(expr) {
                        DataType::Real(float, prec) => {
                            if rnd_prec > 0 {
                                // If rounding precision is positive, than we keep the returned
                                // type as a float. We never return greater precision than was
                                // stored so we choose the minimum of stored precision or rounded
                                // precision.
                                let out_prec = min(*prec, rnd_prec as u8);
                                let rounded_float = (float * 10.0_f64.powf(out_prec as f64))
                                    .round()
                                    / 10.0_f64.powf(out_prec as f64);
                                let real = DataType::try_from(rounded_float).unwrap();
                                Ok(Cow::Owned(real))
                            } else {
                                // Rounding precision is negative, so we need to convert to a
                                // rounded int.
                                let rounded = ((float / 10_f64.powf(-rnd_prec as f64)).round()
                                    * 10_f64.powf(-rnd_prec as f64))
                                    as i32;
                                Ok(Cow::Owned(DataType::Int(rounded)))
                            }
                        }
                        DataType::Int(val) => {
                            let rounded = integer_rnd(*val as i128, rnd_prec) as i32;
                            Ok(Cow::Owned(DataType::Int(rounded)))
                        }
                        DataType::BigInt(val) => {
                            let rounded = integer_rnd(*val as i128, rnd_prec) as i64;
                            Ok(Cow::Owned(DataType::BigInt(rounded)))
                        }
                        DataType::UnsignedInt(val) => {
                            let rounded = integer_rnd(*val as i128, rnd_prec) as u32;
                            Ok(Cow::Owned(DataType::UnsignedInt(rounded)))
                        }
                        DataType::UnsignedBigInt(val) => {
                            let rounded = integer_rnd(*val as i128, rnd_prec) as u64;
                            Ok(Cow::Owned(DataType::UnsignedBigInt(rounded)))
                        }
                        _ => Err(ReadySetError::ProjectExpressionBuiltInFunctionError {
                            function: "round".to_string(),
                            message: "expression does not result in a type that can be rounded."
                                .to_string(),
                        }),
                    }
                }
            },
            CaseWhen {
                condition,
                then_expr,
                else_expr,
            } => {
                if condition.eval(record)?.is_truthy() {
                    then_expr.eval(record)
                } else {
                    else_expr.eval(record)
                }
            }
        }
    }

    pub fn sql_type(
        &self,
        parent_column_type: impl Fn(usize) -> ReadySetResult<Option<SqlType>>,
    ) -> ReadySetResult<Option<SqlType>> {
        // TODO(grfn): Throughout this whole function we basically just assume everything
        // typechecks, which isn't great - but when we actually have a typechecker it'll be
        // attaching types to expressions ahead of time so this is just a stop-gap for now
        match self {
            Expression::Column(c) => parent_column_type(*c),
            Expression::Literal(l) => Ok(l.sql_type()),
            Expression::Op { left, .. } => left.sql_type(parent_column_type),
            Expression::Cast(_, typ) => Ok(Some(typ.clone())),
            Expression::Call(f) => match f {
                BuiltinFunction::ConvertTZ(input, _, _) => input.sql_type(parent_column_type),
                BuiltinFunction::DayOfWeek(_) => Ok(Some(SqlType::Int(None))),
                BuiltinFunction::IfNull(_, y) => y.sql_type(parent_column_type),
                BuiltinFunction::Month(_) => Ok(Some(SqlType::Int(None))),
                BuiltinFunction::Timediff(_, _) => Ok(Some(SqlType::Time)),
                BuiltinFunction::Addtime(e1, _) => e1.sql_type(parent_column_type),
                BuiltinFunction::Round(e1, prec) => match **e1 {
                    Expression::Literal(DataType::Real(_, _)) => {
                        match **prec {
                            // Precision should always be coercable to a DataType::Int.
                            Expression::Literal(DataType::Int(p)) => {
                                if p < 0 {
                                    // Precision is negative, which means that we will be returning a rounded Int.
                                    Ok(Some(SqlType::Int(None)))
                                } else {
                                    // Precision is positive so we will continue to return a Real.
                                    Ok(Some(SqlType::Real))
                                }
                            }
                            Expression::Literal(DataType::BigInt(p)) => {
                                if p < 0 {
                                    // Precision is negative, which means that we will be returning a rounded Int.
                                    Ok(Some(SqlType::Int(None)))
                                } else {
                                    // Precision is positive so we will continue to return a Real.
                                    Ok(Some(SqlType::Real))
                                }
                            }
                            Expression::Literal(DataType::UnsignedInt(_)) => {
                                // Precision is positive so we will continue to return a Real.
                                Ok(Some(SqlType::Real))
                            }
                            Expression::Literal(DataType::UnsignedBigInt(_)) => {
                                // Precision is positive so we will continue to return a Real.
                                Ok(Some(SqlType::Real))
                            }
                            Expression::Literal(DataType::Real(f, _)) => {
                                if f.is_sign_negative() {
                                    // Precision is negative, which means that we will be returning a rounded Int.
                                    Ok(Some(SqlType::Int(None)))
                                } else {
                                    // Precision is positive so we will continue to return a Real.
                                    Ok(Some(SqlType::Real))
                                }
                            }
                            _ => e1.sql_type(parent_column_type),
                        }
                    }
                    // For all other numeric types we always return the same type as they are.
                    Expression::Literal(DataType::UnsignedInt(_)) => {
                        Ok(Some(SqlType::UnsignedInt(None)))
                    }
                    Expression::Literal(DataType::UnsignedBigInt(_)) => {
                        Ok(Some(SqlType::UnsignedBigint(None)))
                    }
                    Expression::Literal(DataType::BigInt(_)) => Ok(Some(SqlType::Bigint(None))),
                    Expression::Literal(DataType::Int(_)) => Ok(Some(SqlType::Int(None))),
                    _ => e1.sql_type(parent_column_type),
                },
            },
            Expression::CaseWhen { then_expr, .. } => then_expr.sql_type(parent_column_type),
        }
    }
}

/// Transforms a `[NaiveDateTime]` into a new one with a different timezone.
/// The `[NaiveDateTime]` is interpreted as having the timezone specified by the
/// `src` parameter, and then it's transformed to timezone specified by the `target` parameter.
pub fn convert_tz(
    datetime: &NaiveDateTime,
    src: &str,
    target: &str,
) -> ReadySetResult<NaiveDateTime> {
    let mk_err = |message: &str| ReadySetError::ProjectExpressionBuiltInFunctionError {
        function: "convert_tz".to_owned(),
        message: message.to_owned(),
    };

    let src_tz: Tz = src
        .parse()
        .map_err(|_| mk_err("Failed to parse the source timezone"))?;
    let target_tz: Tz = target
        .parse()
        .map_err(|_| mk_err("Failed to parse the target timezone"))?;

    let datetime_tz = match src_tz.from_local_datetime(datetime) {
        LocalResult::Single(dt) => dt,
        LocalResult::None => {
            return Err(mk_err(
                "Failed to transform the datetime to a different timezone",
            ))
        }
        LocalResult::Ambiguous(_, _) => {
            return Err(mk_err(
                "Failed to transform the datetime to a different timezone",
            ))
        }
    };

    Ok(datetime_tz.with_timezone(&target_tz).naive_local())
}

fn day_of_week(date: &NaiveDate) -> u8 {
    date.weekday().number_from_sunday() as u8
}

fn month(date: &NaiveDate) -> u8 {
    date.month() as u8
}

fn timediff_datetimes(time1: &NaiveDateTime, time2: &NaiveDateTime) -> MysqlTime {
    let duration = time1.sub(*time2);
    MysqlTime::new(duration)
}

fn timediff_times(time1: &MysqlTime, time2: &MysqlTime) -> MysqlTime {
    time1.sub(*time2)
}

fn addtime_datetime(time1: &NaiveDateTime, time2: &MysqlTime) -> NaiveDateTime {
    time2.add(*time1)
}

fn addtime_times(time1: &MysqlTime, time2: &MysqlTime) -> MysqlTime {
    time1.add(*time2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
    use std::convert::TryInto;
    use std::sync::Arc;
    use test_strategy::proptest;
    use Expression::*;

    #[test]
    fn eval_column() {
        let expr = Column(1);
        assert_eq!(
            expr.eval(&[1.into(), "two".try_into().unwrap()]).unwrap(),
            Cow::Owned("two".try_into().unwrap())
        )
    }

    #[test]
    fn eval_literal() {
        let expr = Literal(1.into());
        assert_eq!(
            expr.eval(&[1.into(), "two".try_into().unwrap()]).unwrap(),
            Cow::Owned(1.into())
        )
    }

    #[test]
    fn eval_add() {
        let expr = Op {
            left: Box::new(Column(0)),
            right: Box::new(Op {
                left: Box::new(Column(1)),
                right: Box::new(Literal(3.into())),
                op: BinaryOperator::Add,
            }),
            op: BinaryOperator::Add,
        };
        assert_eq!(
            expr.eval(&[1.into(), 2.into()]).unwrap(),
            Cow::Owned(6.into())
        );
    }

    #[test]
    fn eval_comparisons() {
        let dt = NaiveDateTime::new(
            NaiveDate::from_ymd(2009, 10, 17),
            NaiveTime::from_hms(12, 0, 0),
        );
        let text_dt: DataType = "2009-10-17 12:00:00".try_into().unwrap();
        let text_less_dt: DataType = "2009-10-16 12:00:00".try_into().unwrap();

        macro_rules! assert_op {
            ($binary_op:expr, $value:expr, $expected:expr) => {
                let expr = Op {
                    left: Box::new(Column(0)),
                    right: Box::new(Literal($value)),
                    op: $binary_op,
                };
                assert_eq!(
                    expr.eval(&[dt.into()]).unwrap(),
                    Cow::Owned($expected.into())
                );
            };
        }
        assert_op!(BinaryOperator::Less, text_less_dt.clone(), 0u8);
        assert_op!(BinaryOperator::Less, text_dt.clone(), 0u8);
        assert_op!(BinaryOperator::LessOrEqual, text_less_dt.clone(), 0u8);
        assert_op!(BinaryOperator::LessOrEqual, text_dt.clone(), 1u8);
        assert_op!(BinaryOperator::Greater, text_less_dt.clone(), 1u8);
        assert_op!(BinaryOperator::Greater, text_dt.clone(), 0u8);
        assert_op!(BinaryOperator::GreaterOrEqual, text_less_dt.clone(), 1u8);
        assert_op!(BinaryOperator::GreaterOrEqual, text_dt.clone(), 1u8);
        assert_op!(BinaryOperator::Equal, text_less_dt, 0u8);
        assert_op!(BinaryOperator::Equal, text_dt, 1u8);
    }

    #[test]
    fn eval_cast() {
        let expr = Cast(Box::new(Column(0)), SqlType::Int(None));
        assert_eq!(
            expr.eval(&["1".try_into().unwrap(), "2".try_into().unwrap()])
                .unwrap(),
            Cow::Owned(1i32.into())
        );
    }

    #[test]
    fn eval_call_convert_tz() {
        let expr = Call(BuiltinFunction::ConvertTZ(
            Box::new(Column(0)),
            Box::new(Column(1)),
            Box::new(Column(2)),
        ));
        let datetime = NaiveDateTime::new(
            NaiveDate::from_ymd(2003, 10, 12),
            NaiveTime::from_hms(5, 13, 33),
        );
        let expected = NaiveDateTime::new(
            NaiveDate::from_ymd(2003, 10, 12),
            NaiveTime::from_hms(11, 58, 33),
        );
        let src = "Atlantic/Cape_Verde";
        let target = "Asia/Kathmandu";
        assert_eq!(
            expr.eval(&[
                datetime.into(),
                src.try_into().unwrap(),
                target.try_into().unwrap()
            ])
            .unwrap(),
            Cow::Owned(expected.into())
        );
        assert_eq!(
            expr.eval(&[
                datetime.into(),
                "invalid timezone".try_into().unwrap(),
                target.try_into().unwrap()
            ])
            .unwrap(),
            Cow::Owned(DataType::None)
        );
        assert_eq!(
            expr.eval(&[
                datetime.into(),
                src.try_into().unwrap(),
                "invalid timezone".try_into().unwrap()
            ])
            .unwrap(),
            Cow::Owned(DataType::None)
        );

        let string_datetime = datetime.to_string();
        assert_eq!(
            expr.eval(&[
                string_datetime.clone().try_into().unwrap(),
                src.try_into().unwrap(),
                target.try_into().unwrap()
            ])
            .unwrap(),
            Cow::Owned(expected.into())
        );

        assert_eq!(
            expr.eval(&[
                string_datetime.clone().try_into().unwrap(),
                "invalid timezone".try_into().unwrap(),
                target.try_into().unwrap()
            ])
            .unwrap(),
            Cow::Owned(DataType::None)
        );
        assert_eq!(
            expr.eval(&[
                string_datetime.try_into().unwrap(),
                src.try_into().unwrap(),
                "invalid timezone".try_into().unwrap()
            ])
            .unwrap(),
            Cow::Owned(DataType::None)
        );
    }

    #[test]
    fn eval_call_day_of_week() {
        let expr = Call(BuiltinFunction::DayOfWeek(Box::new(Column(0))));
        let expected = Cow::Owned(DataType::Int(2));

        let date = NaiveDate::from_ymd(2021, 3, 22); // Monday

        assert_eq!(expr.eval(&[date.into()]).unwrap(), expected);
        assert_eq!(
            expr.eval(&[date.to_string().try_into().unwrap()]).unwrap(),
            expected
        );

        let datetime = NaiveDateTime::new(
            date, // Monday
            NaiveTime::from_hms(18, 8, 00),
        );
        assert_eq!(expr.eval(&[datetime.into()]).unwrap(), expected);
        assert_eq!(
            expr.eval(&[datetime.to_string().try_into().unwrap()])
                .unwrap(),
            expected
        );
    }

    #[test]
    fn eval_call_if_null() {
        let expr = Call(BuiltinFunction::IfNull(
            Box::new(Column(0)),
            Box::new(Column(1)),
        ));
        let value = Cow::Owned(DataType::Int(2));

        assert_eq!(expr.eval(&[DataType::None, 2.into()]).unwrap(), value);
        assert_eq!(expr.eval(&[2.into(), 3.into()]).unwrap(), value);

        let expr2 = Call(BuiltinFunction::IfNull(
            Box::new(Literal(DataType::None)),
            Box::new(Column(0)),
        ));
        assert_eq!(expr2.eval(&[2.into()]).unwrap(), value);

        let expr3 = Call(BuiltinFunction::IfNull(
            Box::new(Column(0)),
            Box::new(Literal(DataType::Int(2))),
        ));
        assert_eq!(expr3.eval(&[DataType::None]).unwrap(), value);
    }

    #[test]
    fn eval_call_month() {
        let expr = Call(BuiltinFunction::Month(Box::new(Column(0))));
        let datetime = NaiveDateTime::new(
            NaiveDate::from_ymd(2003, 10, 12),
            NaiveTime::from_hms(5, 13, 33),
        );
        let expected = 10_u32;
        assert_eq!(
            expr.eval(&[datetime.into()]).unwrap(),
            Cow::Owned(expected.into())
        );
        assert_eq!(
            expr.eval(&[datetime.to_string().try_into().unwrap()])
                .unwrap(),
            Cow::Owned(expected.into())
        );
        assert_eq!(
            expr.eval(&[datetime.date().into()]).unwrap(),
            Cow::Owned(expected.into())
        );
        assert_eq!(
            expr.eval(&[datetime.date().to_string().try_into().unwrap()])
                .unwrap(),
            Cow::Owned(expected.into())
        );
        assert_eq!(
            expr.eval(&["invalid date".try_into().unwrap()]).unwrap(),
            Cow::Owned(DataType::None)
        );
    }

    #[test]
    fn eval_call_timediff() {
        let expr = Call(BuiltinFunction::Timediff(
            Box::new(Column(0)),
            Box::new(Column(1)),
        ));
        let param1 = NaiveDateTime::new(
            NaiveDate::from_ymd(2003, 10, 12),
            NaiveTime::from_hms(5, 13, 33),
        );
        let param2 = NaiveDateTime::new(
            NaiveDate::from_ymd(2003, 10, 14),
            NaiveTime::from_hms(4, 13, 33),
        );
        assert_eq!(
            expr.eval(&[param1.into(), param2.into()]).unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                false, 47, 0, 0, 0
            ))))
        );
        assert_eq!(
            expr.eval(&[
                param1.to_string().try_into().unwrap(),
                param2.to_string().try_into().unwrap()
            ])
            .unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                false, 47, 0, 0, 0
            ))))
        );
        let param1 = NaiveDateTime::new(
            NaiveDate::from_ymd(2003, 10, 12),
            NaiveTime::from_hms(5, 13, 33),
        );
        let param2 = NaiveDateTime::new(
            NaiveDate::from_ymd(2003, 10, 10),
            NaiveTime::from_hms(4, 13, 33),
        );
        assert_eq!(
            expr.eval(&[param1.into(), param2.into()]).unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                true, 49, 0, 0, 0
            ))))
        );
        assert_eq!(
            expr.eval(&[
                param1.to_string().try_into().unwrap(),
                param2.to_string().try_into().unwrap()
            ])
            .unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                true, 49, 0, 0, 0
            ))))
        );
        let param2 = NaiveTime::from_hms(4, 13, 33);
        assert_eq!(
            expr.eval(&[param1.into(), param2.into()]).unwrap(),
            Cow::Owned(DataType::None)
        );
        assert_eq!(
            expr.eval(&[
                param1.to_string().try_into().unwrap(),
                param2.to_string().try_into().unwrap()
            ])
            .unwrap(),
            Cow::Owned(DataType::None)
        );
        let param1 = NaiveTime::from_hms(5, 13, 33);
        assert_eq!(
            expr.eval(&[param1.into(), param2.into()]).unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                true, 1, 0, 0, 0
            ))))
        );
        assert_eq!(
            expr.eval(&[
                param1.to_string().try_into().unwrap(),
                param2.to_string().try_into().unwrap()
            ])
            .unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                true, 1, 0, 0, 0
            ))))
        );
        let param1 = "not a date nor time";
        let param2 = "01:00:00.4";
        assert_eq!(
            expr.eval(&[param1.try_into().unwrap(), param2.try_into().unwrap()])
                .unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                false, 1, 0, 0, 400_000
            ))))
        );
        assert_eq!(
            expr.eval(&[param2.try_into().unwrap(), param1.try_into().unwrap()])
                .unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                true, 1, 0, 0, 400_000
            ))))
        );

        let param2 = "10000.4";
        assert_eq!(
            expr.eval(&[param1.try_into().unwrap(), param2.try_into().unwrap()])
                .unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                false, 1, 0, 0, 400_000
            ))))
        );
        assert_eq!(
            expr.eval(&[param2.try_into().unwrap(), param1.try_into().unwrap()])
                .unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                true, 1, 0, 0, 400_000
            ))))
        );

        let param2 = 3.57;
        assert_eq!(
            expr.eval(&[
                DataType::try_from(param1).unwrap(),
                DataType::try_from(param2).unwrap()
            ])
            .unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_microseconds(
                (-param2 * 1_000_000_f64) as i64
            ))))
        );
    }

    #[test]
    fn eval_call_addtime() {
        let expr = Call(BuiltinFunction::Addtime(
            Box::new(Column(0)),
            Box::new(Column(1)),
        ));
        let param1 = NaiveDateTime::new(
            NaiveDate::from_ymd(2003, 10, 12),
            NaiveTime::from_hms(5, 13, 33),
        );
        let param2 = NaiveDateTime::new(
            NaiveDate::from_ymd(2003, 10, 14),
            NaiveTime::from_hms(4, 13, 33),
        );
        assert_eq!(
            expr.eval(&[param1.into(), param2.into()]).unwrap(),
            Cow::Owned(DataType::None)
        );
        assert_eq!(
            expr.eval(&[
                param1.to_string().try_into().unwrap(),
                param2.to_string().try_into().unwrap()
            ])
            .unwrap(),
            Cow::Owned(DataType::None)
        );
        let param2 = NaiveTime::from_hms(4, 13, 33);
        assert_eq!(
            expr.eval(&[param1.into(), param2.into()]).unwrap(),
            Cow::Owned(DataType::Timestamp(NaiveDateTime::new(
                NaiveDate::from_ymd(2003, 10, 12),
                NaiveTime::from_hms(9, 27, 6),
            )))
        );
        assert_eq!(
            expr.eval(&[
                param1.to_string().try_into().unwrap(),
                param2.to_string().try_into().unwrap()
            ])
            .unwrap(),
            Cow::Owned(DataType::Timestamp(NaiveDateTime::new(
                NaiveDate::from_ymd(2003, 10, 12),
                NaiveTime::from_hms(9, 27, 6),
            )))
        );
        let param2 = MysqlTime::from_hmsus(false, 3, 11, 35, 0);
        assert_eq!(
            expr.eval(&[param1.into(), param2.into()]).unwrap(),
            Cow::Owned(DataType::Timestamp(NaiveDateTime::new(
                NaiveDate::from_ymd(2003, 10, 12),
                NaiveTime::from_hms(2, 1, 58),
            )))
        );
        assert_eq!(
            expr.eval(&[
                param1.to_string().try_into().unwrap(),
                param2.to_string().try_into().unwrap()
            ])
            .unwrap(),
            Cow::Owned(DataType::Timestamp(NaiveDateTime::new(
                NaiveDate::from_ymd(2003, 10, 12),
                NaiveTime::from_hms(2, 1, 58),
            )))
        );
        let param1 = MysqlTime::from_hmsus(true, 10, 12, 44, 123_000);
        assert_eq!(
            expr.eval(&[param2.into(), param1.into()]).unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                true, 7, 1, 9, 123_000
            ))))
        );
        assert_eq!(
            expr.eval(&[
                param2.to_string().try_into().unwrap(),
                param1.to_string().try_into().unwrap()
            ])
            .unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                true, 7, 1, 9, 123_000
            ))))
        );
        let param1 = "not a date nor time";
        let param2 = "01:00:00.4";
        assert_eq!(
            expr.eval(&[param1.try_into().unwrap(), param2.try_into().unwrap()])
                .unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                true, 1, 0, 0, 400_000
            ))))
        );
        assert_eq!(
            expr.eval(&[param2.try_into().unwrap(), param1.try_into().unwrap()])
                .unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                true, 1, 0, 0, 400_000
            ))))
        );

        let param2 = "10000.4";
        assert_eq!(
            expr.eval(&[param1.try_into().unwrap(), param2.try_into().unwrap()])
                .unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                true, 1, 0, 0, 400_000
            ))))
        );
        assert_eq!(
            expr.eval(&[param2.try_into().unwrap(), param1.try_into().unwrap()])
                .unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_hmsus(
                true, 1, 0, 0, 400_000
            ))))
        );

        let param2 = 3.57;
        assert_eq!(
            expr.eval(&[
                param1.try_into().unwrap(),
                DataType::try_from(param2).unwrap()
            ])
            .unwrap(),
            Cow::Owned(DataType::Time(Arc::new(MysqlTime::from_microseconds(
                (param2 * 1_000_000_f64) as i64
            ))))
        );
    }

    #[test]
    fn eval_call_round() {
        let expr = Call(BuiltinFunction::Round(
            Box::new(Column(0)),
            Box::new(Column(1)),
        ));
        let number = 4.12345;
        let precision = 3;
        let param1 = DataType::try_from(number).unwrap();
        let param2 = DataType::Int(precision);
        let want = Cow::Owned(DataType::try_from(4.123_f64).unwrap());
        assert_eq!(expr.eval(&[param1, param2]).unwrap(), want,);
    }

    #[test]
    fn eval_call_round_with_negative_precision() {
        let expr = Call(BuiltinFunction::Round(
            Box::new(Column(0)),
            Box::new(Column(1)),
        ));
        let number = 52.12345;
        let precision = -1;
        let param1 = DataType::try_from(number).unwrap();
        let param2 = DataType::Int(precision);
        let want = Cow::Owned(DataType::try_from(50).unwrap());
        assert_eq!(expr.eval(&[param1, param2]).unwrap(), want,);
    }

    #[test]
    fn eval_call_round_with_float_precision() {
        let expr = Call(BuiltinFunction::Round(
            Box::new(Column(0)),
            Box::new(Column(1)),
        ));
        let number = 52.12345;
        let precision = -1.0_f64;
        let param1 = DataType::try_from(number).unwrap();
        let param2 = DataType::try_from(precision).unwrap();
        let want = Cow::Owned(DataType::try_from(50).unwrap());
        assert_eq!(expr.eval(&[param1, param2]).unwrap(), want,);
    }

    // This is actually straight from MySQL:
    // mysql> SELECT ROUND(123.3, "banana");
    // +------------------------+
    // | ROUND(123.3, "banana") |
    // +------------------------+
    // |                    123 |
    // +------------------------+
    // 1 row in set, 2 warnings (0.00 sec)
    #[test]
    fn eval_call_round_with_banana() {
        let expr = Call(BuiltinFunction::Round(
            Box::new(Column(0)),
            Box::new(Column(1)),
        ));
        let number = 52.12345;
        let precision = "banana";
        let param1 = DataType::try_from(number).unwrap();
        let param2 = DataType::try_from(precision).unwrap();
        let want = Cow::Owned(DataType::try_from(52).unwrap());
        assert_eq!(expr.eval(&[param1, param2]).unwrap(), want,);
    }

    #[test]
    fn month_null() {
        let expr = Call(BuiltinFunction::Month(Box::new(Column(0))));
        assert_eq!(
            expr.eval(&[DataType::None]).unwrap(),
            Cow::Owned(DataType::None)
        );
    }

    #[test]
    fn value_truthiness() {
        assert_eq!(
            Expression::Op {
                left: Box::new(Expression::Literal(1.into())),
                op: BinaryOperator::And,
                right: Box::new(Expression::Literal(3.into())),
            }
            .eval(&[])
            .unwrap(),
            Cow::Owned(1.into())
        );

        assert_eq!(
            Expression::Op {
                left: Box::new(Expression::Literal(1.into())),
                op: BinaryOperator::And,
                right: Box::new(Expression::Literal(0.into())),
            }
            .eval(&[])
            .unwrap(),
            Cow::Owned(0.into())
        );
    }

    #[test]
    fn eval_case_when() {
        let expr = Expression::CaseWhen {
            condition: Box::new(Op {
                left: Box::new(Expression::Column(0)),
                op: BinaryOperator::Equal,
                right: Box::new(Expression::Literal(1.into())),
            }),
            then_expr: Box::new(Expression::Literal("yes".try_into().unwrap())),
            else_expr: Box::new(Expression::Literal("no".try_into().unwrap())),
        };

        assert_eq!(
            expr.eval(&[1.into()]).unwrap().as_ref(),
            &DataType::try_from("yes").unwrap()
        );

        assert_eq!(
            expr.eval(&[8.into()]).unwrap().as_ref(),
            &DataType::try_from("no").unwrap()
        );
    }

    #[test]
    fn like_expr() {
        let expr = Expression::Op {
            left: Box::new(Expression::Literal("foo".into())),
            op: BinaryOperator::Like,
            right: Box::new(Expression::Literal("f%".into())),
        };
        let res = expr.eval(&[]).unwrap();
        assert!(res.is_truthy());
    }

    mod builtin_funcs {
        use super::*;
        use launchpad::arbitrary::arbitrary_timestamp_naive_date_time;

        // NOTE(Fran): We have to be careful when testing timezones, as the time difference
        //   between two timezones might differ depending on the date (due to daylight savings
        //   or by historical changes).
        #[proptest]
        fn convert_tz(#[strategy(arbitrary_timestamp_naive_date_time())] datetime: NaiveDateTime) {
            let src = "Atlantic/Cape_Verde";
            let target = "Asia/Kathmandu";
            let src_tz: Tz = src.parse().unwrap();
            let target_tz: Tz = target.parse().unwrap();
            let expected = src_tz
                .yo_opt(datetime.year(), datetime.ordinal())
                .and_hms_opt(datetime.hour(), datetime.minute(), datetime.second())
                .unwrap()
                .with_timezone(&target_tz)
                .naive_local();
            assert_eq!(super::convert_tz(&datetime, src, target).unwrap(), expected);
            assert!(super::convert_tz(&datetime, "invalid timezone", target).is_err());
            assert!(super::convert_tz(&datetime, src, "invalid timezone").is_err());
        }

        #[proptest]
        fn day_of_week(#[strategy(arbitrary_timestamp_naive_date_time())] datetime: NaiveDateTime) {
            let expected = datetime.weekday().number_from_sunday() as u8;
            assert_eq!(super::day_of_week(&datetime.date()), expected);
        }

        #[proptest]
        fn month(#[strategy(arbitrary_timestamp_naive_date_time())] datetime: NaiveDateTime) {
            let expected = datetime.month() as u8;
            assert_eq!(super::month(&datetime.date()), expected);
        }
    }
}
