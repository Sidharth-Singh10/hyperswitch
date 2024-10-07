use api_models::payments::{HeaderPayload, PaymentsConfirmIntentRequest};
use api_models::{
    admin::ExtendedCardInfoConfig,
    enums::FrmSuggestion,
    // payment_methods::PaymentMethodsData,
    payments::{ExtendedCardInfo, GetAddressFromPaymentMethodData},
};
use async_trait::async_trait;
use error_stack::ResultExt;
use hyperswitch_domain_models::{
    merchant_account::MerchantAccount,
    merchant_key_store::MerchantKeyStore,
    payments::{payment_attempt::PaymentAttempt, PaymentConfirmData, PaymentIntent},
};
use router_env::{instrument, tracing};
use tracing_futures::Instrument;

use super::{Domain, GetTracker, GetTrackerResponse, Operation, UpdateTracker, ValidateRequest};
use crate::{
    core::{
        authentication,
        errors::{self, CustomResult, RouterResult, StorageErrorExt},
        payments::{
            self, helpers, operations, populate_surcharge_details, CustomerDetails, PaymentAddress,
            PaymentData,
        },
        utils as core_utils,
    },
    routes::{app::ReqState, SessionState},
    services,
    types::{
        self,
        api::{self, ConnectorCallType, PaymentIdTypeExt},
        domain::{self},
        storage::{self, enums as storage_enums},
    },
    utils::{self, OptionExt},
};

trait PaymentsConfirmIntentBridge {
    async fn create_domain_model_from_request(
        &self,
        state: &SessionState,
        payment_intent: &PaymentIntent,
        storage_scheme: storage_enums::MerchantStorageScheme,
    ) -> RouterResult<PaymentAttempt>;
}

impl PaymentsConfirmIntentBridge for api_models::payments::PaymentsConfirmIntentRequest {
    async fn create_domain_model_from_request(
        &self,
        state: &SessionState,
        payment_intent: &PaymentIntent,
        storage_scheme: storage_enums::MerchantStorageScheme,
    ) -> RouterResult<PaymentAttempt> {
        let now = common_utils::date_time::now();
        let cell_id = state.conf.cell_information.id.clone();

        // TODO: generate attempt id from intent id based on the merchant config for retries
        let id = common_utils::id_type::GlobalAttemptId::generate(&cell_id);
        let intent_amount_details = payment_intent.amount_details.clone();

        // TODO: move this to a impl function
        let attempt_amount_details =
            hyperswitch_domain_models::payments::payment_attempt::AttemptAmountDetails {
                net_amount: intent_amount_details.order_amount,
                amount_to_capture: None,
                surcharge_amount: None,
                tax_on_surcharge: None,
                amount_capturable: common_utils::types::MinorUnit::new(0),
                shipping_cost: None,
                order_tax_amount: None,
            };

        Ok(PaymentAttempt {
            payment_id: payment_intent.id.clone(),
            merchant_id: payment_intent.merchant_id.clone(),
            amount_details: attempt_amount_details,
            status: common_enums::AttemptStatus::Started,
            connector: None,
            authentication_type: payment_intent.authentication_type.clone(),
            created_at: now,
            modified_at: now,
            last_synced: None,
            cancellation_reason: None,
            browser_info: None,
            payment_token: None,
            connector_metadata: None,
            payment_experience: None,
            payment_method_data: None,
            routing_result: None,
            preprocessing_step_id: None,
            multiple_capture_count: None,
            connector_response_reference_id: None,
            updated_by: storage_scheme.to_string(),
            authentication_data: None,
            encoded_data: None,
            merchant_connector_id: None,
            external_three_ds_authentication_attempted: None,
            authentication_connector: None,
            authentication_id: None,
            fingerprint_id: None,
            charge_id: None,
            client_source: None,
            client_version: None,
            customer_acceptance: None,
            profile_id: payment_intent.profile_id.clone(),
            organization_id: payment_intent.organization_id.clone(),
            payment_method_type: self.payment_method_type.clone(),
            payment_method_id: None,
            connector_payment_id: None,
            payment_method_subtype: self.payment_method_subtype,
            authentication_applied: None,
            external_reference_id: None,
            payment_method_billing_address: None,
            error: None,
            id,
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PaymentsIntentConfirm;

type BoxedConfirmOperation<'b, F> =
    super::BoxedOperation<'b, F, PaymentsConfirmIntentRequest, PaymentConfirmData<F>>;

// TODO: change the macro to include changes for v2
// TODO: PaymentData in the macro should be an input
impl<F: Send + Clone> Operation<F, PaymentsConfirmIntentRequest> for &PaymentsIntentConfirm {
    type Data = PaymentConfirmData<F>;
    fn to_validate_request(
        &self,
    ) -> RouterResult<
        &(dyn ValidateRequest<F, PaymentsConfirmIntentRequest, Self::Data> + Send + Sync),
    > {
        Ok(*self)
    }
    fn to_get_tracker(
        &self,
    ) -> RouterResult<&(dyn GetTracker<F, Self::Data, PaymentsConfirmIntentRequest> + Send + Sync)>
    {
        Ok(*self)
    }
    fn to_domain(
        &self,
    ) -> RouterResult<&(dyn Domain<F, PaymentsConfirmIntentRequest, Self::Data>)> {
        Ok(*self)
    }
    fn to_update_tracker(
        &self,
    ) -> RouterResult<&(dyn UpdateTracker<F, Self::Data, PaymentsConfirmIntentRequest> + Send + Sync)>
    {
        Ok(*self)
    }
}
#[automatically_derived]
impl<F: Send + Clone> Operation<F, PaymentsConfirmIntentRequest> for PaymentsIntentConfirm {
    type Data = PaymentConfirmData<F>;
    fn to_validate_request(
        &self,
    ) -> RouterResult<
        &(dyn ValidateRequest<F, PaymentsConfirmIntentRequest, Self::Data> + Send + Sync),
    > {
        Ok(self)
    }
    fn to_get_tracker(
        &self,
    ) -> RouterResult<&(dyn GetTracker<F, Self::Data, PaymentsConfirmIntentRequest> + Send + Sync)>
    {
        Ok(self)
    }
    fn to_domain(&self) -> RouterResult<&dyn Domain<F, PaymentsConfirmIntentRequest, Self::Data>> {
        Ok(self)
    }
    fn to_update_tracker(
        &self,
    ) -> RouterResult<&(dyn UpdateTracker<F, Self::Data, PaymentsConfirmIntentRequest> + Send + Sync)>
    {
        Ok(self)
    }
}

impl<F: Send + Clone> ValidateRequest<F, PaymentsConfirmIntentRequest, PaymentConfirmData<F>>
    for PaymentsIntentConfirm
{
    #[instrument(skip_all)]
    fn validate_request<'a, 'b>(
        &'b self,
        request: &PaymentsConfirmIntentRequest,
        merchant_account: &'a domain::MerchantAccount,
    ) -> RouterResult<(BoxedConfirmOperation<'b, F>, operations::ValidateResult)> {
        let validate_result = operations::ValidateResult {
            merchant_id: merchant_account.get_id().to_owned(),
            storage_scheme: merchant_account.storage_scheme,
            requeue: false,
        };

        Ok((Box::new(self), validate_result))
    }
}

#[async_trait]
impl<F: Send + Clone> GetTracker<F, PaymentConfirmData<F>, PaymentsConfirmIntentRequest>
    for PaymentsIntentConfirm
{
    #[instrument(skip_all)]
    async fn get_trackers<'a>(
        &'a self,
        state: &'a SessionState,
        payment_id: &common_utils::id_type::GlobalPaymentId,
        request: &PaymentsConfirmIntentRequest,
        merchant_account: &MerchantAccount,
        profile: &domain::Profile,
        key_store: &MerchantKeyStore,
        header_payload: &HeaderPayload,
    ) -> RouterResult<GetTrackerResponse<'a, F, PaymentsConfirmIntentRequest, PaymentConfirmData<F>>>
    {
        let db = &*state.store;
        let key_manager_state = &state.into();

        let storage_scheme = merchant_account.storage_scheme;

        let payment_intent = db
            .find_payment_intent_by_id(key_manager_state, payment_id, key_store, storage_scheme)
            .await
            .to_not_found_response(errors::ApiErrorResponse::PaymentNotFound)?;

        let payment_attempt_domain_model = request
            .create_domain_model_from_request(&state, &payment_intent, storage_scheme)
            .await?;

        let payment_attempt = db
            .insert_payment_attempt(
                key_manager_state,
                key_store,
                payment_attempt_domain_model,
                storage_scheme,
            )
            .await
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Could not insert payment attempt")?;

        let profile_id = &payment_intent.profile_id;

        let profile = db
            .find_business_profile_by_profile_id(&(state).into(), key_store, profile_id)
            .await
            .to_not_found_response(errors::ApiErrorResponse::ProfileNotFound {
                id: profile_id.get_string_repr().to_owned(),
            })?;

        let payment_method_data =
            hyperswitch_domain_models::payment_method_data::PaymentMethodData::from(
                request.payment_method_data.payment_method_data.clone(),
            );

        let payment_data = PaymentConfirmData {
            flow: std::marker::PhantomData,
            payment_intent,
            payment_attempt,
            payment_method_data: Some(payment_method_data),
        };

        let get_trackers_response = operations::GetTrackerResponse {
            operation: Box::new(self),
            customer_details: None,
            payment_data,
            mandate_type: None,
        };

        Ok(get_trackers_response)
    }
}

#[async_trait]
impl<F: Clone + Send> Domain<F, PaymentsConfirmIntentRequest, PaymentConfirmData<F>>
    for PaymentsIntentConfirm
{
    async fn get_customer_details<'a>(
        &'a self,
        state: &SessionState,
        payment_data: &mut PaymentConfirmData<F>,
        request: Option<CustomerDetails>,
        merchant_key_store: &MerchantKeyStore,
        storage_scheme: storage_enums::MerchantStorageScheme,
    ) -> CustomResult<(BoxedConfirmOperation<'a, F>, Option<domain::Customer>), errors::StorageError>
    {
        Ok((Box::new(self), None))
    }

    #[instrument(skip_all)]
    async fn make_pm_data<'a>(
        &'a self,
        state: &'a SessionState,
        payment_data: &mut PaymentConfirmData<F>,
        storage_scheme: storage_enums::MerchantStorageScheme,
        key_store: &MerchantKeyStore,
        customer: &Option<domain::Customer>,
        business_profile: &domain::Profile,
    ) -> RouterResult<(
        BoxedConfirmOperation<'a, F>,
        Option<domain::PaymentMethodData>,
        Option<String>,
    )> {
        Ok((Box::new(self), None, None))
    }

    async fn get_connector<'a>(
        &'a self,
        _merchant_account: &domain::MerchantAccount,
        state: &SessionState,
        request: &PaymentsConfirmIntentRequest,
        _payment_intent: &storage::PaymentIntent,
        _key_store: &domain::MerchantKeyStore,
    ) -> CustomResult<api::ConnectorChoice, errors::ApiErrorResponse> {
        todo!()
    }
}

#[async_trait]
impl<F: Clone> UpdateTracker<F, PaymentConfirmData<F>, PaymentsConfirmIntentRequest>
    for PaymentsIntentConfirm
{
    #[instrument(skip_all)]
    async fn update_trackers<'b>(
        &'b self,
        state: &'b SessionState,
        req_state: ReqState,
        mut payment_data: PaymentConfirmData<F>,
        customer: Option<domain::Customer>,
        storage_scheme: storage_enums::MerchantStorageScheme,
        updated_customer: Option<storage::CustomerUpdate>,
        key_store: &domain::MerchantKeyStore,
        frm_suggestion: Option<FrmSuggestion>,
        header_payload: api::HeaderPayload,
    ) -> RouterResult<(BoxedConfirmOperation<'b, F>, PaymentConfirmData<F>)>
    where
        F: 'b + Send,
    {
        let db = &*state.store;
        let key_manager_state = &state.into();

        let intent_status = common_enums::IntentStatus::Processing;
        let attempt_status = common_enums::AttemptStatus::Pending;

        let connector = payment_data
            .payment_attempt
            .connector
            .clone()
            .get_required_value("connector")
            .attach_printable("Connector is none when constructing response")?;

        let merchant_connector_id = payment_data
            .payment_attempt
            .merchant_connector_id
            .clone()
            .get_required_value("merchant_connector_id")
            .attach_printable("Merchant connector id is none when constructing response")?;

        let payment_intent_update =
            hyperswitch_domain_models::payments::payment_intent::PaymentIntentUpdate::ConfirmIntent {
                status: intent_status,
                updated_by: storage_scheme.to_string(),
            };

        let payment_attempt_update = hyperswitch_domain_models::payments::payment_attempt::PaymentAttemptUpdate::ConfirmIntent {
            status: attempt_status,
            updated_by: storage_scheme.to_string(),
            connector: connector,
            merchant_connector_id: merchant_connector_id,
        };

        // let conector_request_reference_id = payment_data.payment_attempt.id.get_string_repr();

        let current_payment_intent = payment_data.payment_intent.clone();
        let updated_payment_intent = db
            .update_payment_intent(
                key_manager_state,
                current_payment_intent,
                payment_intent_update,
                key_store,
                storage_scheme,
            )
            .await
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Unable to update payment intent")?;
        payment_data.payment_intent = updated_payment_intent;

        let current_payment_attempt = payment_data.payment_attempt.clone();
        let updated_payment_attempt = db
            .update_payment_attempt(
                key_manager_state,
                key_store,
                current_payment_attempt,
                payment_attempt_update,
                storage_scheme,
            )
            .await
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Unable to update payment attempt")?;

        payment_data.payment_attempt = updated_payment_attempt;

        Ok((Box::new(self), payment_data))
    }
}
