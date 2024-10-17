use std::collections::HashSet;

use api_models::analytics::{
    payments::{PaymentDimensions, PaymentFilters, PaymentMetricsBucketIdentifier},
    Granularity, TimeRange,
};
use common_utils::errors::ReportSwitchExt;
use diesel_models::enums as storage_enums;
use error_stack::ResultExt;
use time::PrimitiveDateTime;

use super::PaymentMetricRow;
use crate::{
    enums::AuthInfo,
    query::{
        Aggregate, FilterTypes, GroupByClause, Order, QueryBuilder, QueryFilter, SeriesBucket,
        ToSql, Window,
    },
    types::{AnalyticsCollection, AnalyticsDataSource, MetricsError, MetricsResult},
};

#[derive(Default)]
pub(crate) struct FailureReasons;

#[async_trait::async_trait]
impl<T> super::PaymentMetric<T> for FailureReasons
where
    T: AnalyticsDataSource + super::PaymentMetricAnalytics,
    PrimitiveDateTime: ToSql<T>,
    AnalyticsCollection: ToSql<T>,
    Granularity: GroupByClause<T>,
    Aggregate<&'static str>: ToSql<T>,
    Window<&'static str>: ToSql<T>,
{
    async fn load_metrics(
        &self,
        dimensions: &[PaymentDimensions],
        auth: &AuthInfo,
        filters: &PaymentFilters,
        granularity: &Option<Granularity>,
        time_range: &TimeRange,
        pool: &T,
    ) -> MetricsResult<HashSet<(PaymentMetricsBucketIdentifier, PaymentMetricRow)>> {
        let mut inner_query_builder: QueryBuilder<T> =
            QueryBuilder::new(AnalyticsCollection::PaymentSessionized);
        inner_query_builder
            .add_select_column("sum(sign_flag)")
            .switch()?;

        inner_query_builder
            .add_custom_filter_clause(
                PaymentDimensions::ErrorReason,
                "NULL",
                FilterTypes::IsNotNull,
            )
            .switch()?;

        time_range
            .set_filter_clause(&mut inner_query_builder)
            .attach_printable("Error filtering time range for inner query")
            .switch()?;

        let inner_query_string = inner_query_builder
            .build_query()
            .attach_printable("Error building inner query")
            .change_context(MetricsError::QueryBuildingError)?;

        let mut outer_query_builder: QueryBuilder<T> =
            QueryBuilder::new(AnalyticsCollection::PaymentSessionized);

        for dim in dimensions.iter() {
            outer_query_builder.add_select_column(dim).switch()?;
        }

        outer_query_builder
            .add_select_column("sum(sign_flag) AS count")
            .switch()?;

        outer_query_builder
            .add_select_column(format!("({}) AS total", inner_query_string))
            .switch()?;

        outer_query_builder
            .add_select_column("first_attempt")
            .switch()?;

        outer_query_builder
            .add_select_column(Aggregate::Min {
                field: "created_at",
                alias: Some("start_bucket"),
            })
            .switch()?;

        outer_query_builder
            .add_select_column(Aggregate::Max {
                field: "created_at",
                alias: Some("end_bucket"),
            })
            .switch()?;

        filters
            .set_filter_clause(&mut outer_query_builder)
            .switch()?;

        auth.set_filter_clause(&mut outer_query_builder).switch()?;

        time_range
            .set_filter_clause(&mut outer_query_builder)
            .attach_printable("Error filtering time range for outer query")
            .switch()?;

        outer_query_builder
            .add_filter_clause(
                PaymentDimensions::PaymentStatus,
                storage_enums::AttemptStatus::Failure,
            )
            .switch()?;

        outer_query_builder
            .add_custom_filter_clause(
                PaymentDimensions::ErrorReason,
                "NULL",
                FilterTypes::IsNotNull,
            )
            .switch()?;

        for dim in dimensions.iter() {
            outer_query_builder
                .add_group_by_clause(dim)
                .attach_printable("Error grouping by dimensions")
                .switch()?;
        }

        outer_query_builder
            .add_group_by_clause("first_attempt")
            .attach_printable("Error grouping by first_attempt")
            .switch()?;

        if let Some(granularity) = granularity.as_ref() {
            granularity
                .set_group_by_clause(&mut outer_query_builder)
                .attach_printable("Error adding granularity")
                .switch()?;
        }

        outer_query_builder
            .add_order_by_clause("count", Order::Descending)
            .attach_printable("Error adding order by clause")
            .switch()?;

        for dim in dimensions.iter() {
            if dim != &PaymentDimensions::ErrorReason {
                outer_query_builder
                    .add_order_by_clause(dim, Order::Ascending)
                    .attach_printable("Error adding order by clause")
                    .switch()?;
            }
        }

        outer_query_builder
            .set_limit_by(5, &[PaymentDimensions::Connector])
            .attach_printable("Error adding limit clause")
            .switch()?;

        outer_query_builder
            .execute_query::<PaymentMetricRow, _>(pool)
            .await
            .change_context(MetricsError::QueryBuildingError)?
            .change_context(MetricsError::QueryExecutionFailure)?
            .into_iter()
            .map(|i| {
                Ok((
                    PaymentMetricsBucketIdentifier::new(
                        i.currency.as_ref().map(|i| i.0),
                        None,
                        i.connector.clone(),
                        i.authentication_type.as_ref().map(|i| i.0),
                        i.payment_method.clone(),
                        i.payment_method_type.clone(),
                        i.client_source.clone(),
                        i.client_version.clone(),
                        i.profile_id.clone(),
                        i.card_network.clone(),
                        i.merchant_id.clone(),
                        i.card_last_4.clone(),
                        i.card_issuer.clone(),
                        i.error_reason.clone(),
                        TimeRange {
                            start_time: match (granularity, i.start_bucket) {
                                (Some(g), Some(st)) => g.clip_to_start(st)?,
                                _ => time_range.start_time,
                            },
                            end_time: granularity.as_ref().map_or_else(
                                || Ok(time_range.end_time),
                                |g| i.end_bucket.map(|et| g.clip_to_end(et)).transpose(),
                            )?,
                        },
                    ),
                    i,
                ))
            })
            .collect::<error_stack::Result<
                HashSet<(PaymentMetricsBucketIdentifier, PaymentMetricRow)>,
                crate::query::PostProcessingError,
            >>()
            .change_context(MetricsError::PostProcessingFailure)
    }
}