package com.oxidt.entitle.capacitor

import com.android.billingclient.api.BillingClient
import com.android.billingclient.api.BillingClient.BillingResponseCode
import com.android.billingclient.api.BillingClient.ProductType
import com.android.billingclient.api.BillingClientStateListener
import com.android.billingclient.api.BillingFlowParams
import com.android.billingclient.api.BillingResult
import com.android.billingclient.api.PendingPurchasesParams
import com.android.billingclient.api.ProductDetails
import com.android.billingclient.api.Purchase
import com.android.billingclient.api.QueryProductDetailsParams
import com.android.billingclient.api.QueryPurchasesParams
import com.getcapacitor.JSArray
import com.getcapacitor.JSObject
import com.getcapacitor.Plugin
import com.getcapacitor.PluginCall
import com.getcapacitor.PluginMethod
import com.getcapacitor.annotation.CapacitorPlugin
import java.math.BigDecimal
import java.math.RoundingMode
import java.util.Currency

/**
 * Play Billing flows for entitle. Never acknowledges or consumes a purchase: the entitle server
 * acknowledges it after verifying the synced purchase token.
 */
@CapacitorPlugin(name = "EntitleStore")
class EntitleStorePlugin : Plugin() {
    private class InFlight(val call: PluginCall, val productId: String)

    private lateinit var billing: BillingClient
    private val connectionWaiters = mutableListOf<(BillingResult) -> Unit>()

    /** The `purchase()` call waiting for its billing flow result. */
    @Volatile
    private var inFlight: InFlight? = null

    override fun load() {
        billing = BillingClient.newBuilder(context)
            .setListener(::onPurchasesUpdated)
            .enablePendingPurchases(PendingPurchasesParams.newBuilder().enableOneTimeProducts().build())
            .build()
    }

    override fun handleOnDestroy() {
        billing.endConnection()
    }

    @PluginMethod
    fun getProducts(call: PluginCall) {
        val ids = call.getArray("productIds")?.toList<String>() ?: emptyList()
        if (ids.isEmpty()) {
            call.resolve(JSObject().apply { put("products", JSArray()) })
            return
        }
        connected(call) {
            queryDetails(call, ids) { details ->
                call.resolve(JSObject().apply { put("products", JSArray(details.mapNotNull(::productJson))) })
            }
        }
    }

    @PluginMethod
    fun purchase(call: PluginCall) {
        val productId = call.getString("productId")
        val accountToken = call.getString("accountToken")
        if (productId == null || accountToken == null) {
            call.reject("productId and accountToken are required", "INVALID_ARGUMENT")
            return
        }
        if (inFlight != null) {
            call.reject("another purchase is in progress", "PURCHASE_IN_PROGRESS")
            return
        }
        connected(call) {
            queryDetails(call, listOf(productId)) { details ->
                val product = details.firstOrNull()
                if (product == null) {
                    call.reject("product not found: $productId", "PRODUCT_NOT_FOUND")
                    return@queryDetails
                }
                val offerToken = call.getString("offerToken") ?: defaultOfferToken(product)
                val productParams = BillingFlowParams.ProductDetailsParams.newBuilder()
                    .setProductDetails(product)
                    .apply { if (offerToken != null) setOfferToken(offerToken) }
                    .build()
                val flowParams = BillingFlowParams.newBuilder()
                    .setProductDetailsParamsList(listOf(productParams))
                    .setObfuscatedAccountId(accountToken)
                    .build()
                val activity = activity
                if (activity == null) {
                    call.reject("no activity to show the purchase flow", "STORE_ERROR")
                    return@queryDetails
                }
                activity.runOnUiThread {
                    inFlight = InFlight(call, productId)
                    val result = billing.launchBillingFlow(activity, flowParams)
                    if (result.responseCode != BillingResponseCode.OK) {
                        inFlight = null
                        reject(call, result)
                    }
                }
            }
        }
    }

    /** iOS only; Play purchases are acknowledged by the entitle server. */
    @PluginMethod
    fun finishTransaction(call: PluginCall) {
        call.resolve()
    }

    @PluginMethod
    fun getCurrentPurchases(call: PluginCall) {
        connected(call) {
            val owned = JSArray()
            fun query(types: List<String>) {
                val type = types.firstOrNull()
                if (type == null) {
                    call.resolve(JSObject().apply { put("purchases", owned) })
                    return
                }
                val params = QueryPurchasesParams.newBuilder().setProductType(type).build()
                billing.queryPurchasesAsync(params) { result, purchases ->
                    if (result.responseCode != BillingResponseCode.OK) {
                        reject(call, result)
                        return@queryPurchasesAsync
                    }
                    purchases
                        .filter { it.purchaseState == Purchase.PurchaseState.PURCHASED }
                        .forEach { owned.put(purchaseJson(it)) }
                    query(types.drop(1))
                }
            }
            query(listOf(ProductType.SUBS, ProductType.INAPP))
        }
    }

    private fun onPurchasesUpdated(result: BillingResult, purchases: List<Purchase>?) {
        val waiting = inFlight
        val mine = waiting?.let { w -> purchases?.firstOrNull { w.productId in it.products } }
        // Purchases not started by purchase(), e.g. a pending purchase that completed.
        purchases.orEmpty()
            .filter { it !== mine && it.purchaseState == Purchase.PurchaseState.PURCHASED }
            .forEach { notifyListeners("purchaseUpdated", purchaseJson(it), true) }

        if (waiting == null) return
        val code = result.responseCode
        if (code == BillingResponseCode.OK && mine == null) return // an update unrelated to the open flow
        inFlight = null
        val call = waiting.call
        when {
            code == BillingResponseCode.USER_CANCELED -> call.resolve(JSObject().apply { put("status", "cancelled") })
            code != BillingResponseCode.OK -> reject(call, result)
            mine!!.purchaseState == Purchase.PurchaseState.PURCHASED -> call.resolve(
                JSObject().apply {
                    put("status", "purchased")
                    put("purchase", purchaseJson(mine))
                },
            )
            else -> call.resolve(JSObject().apply { put("status", "pending") })
        }
    }

    /** Runs `block` once the billing client is connected, connecting first if needed. */
    private fun connected(call: PluginCall, block: () -> Unit) {
        if (billing.isReady) {
            block()
            return
        }
        synchronized(connectionWaiters) {
            connectionWaiters.add { result ->
                if (result.responseCode == BillingResponseCode.OK) block() else reject(call, result)
            }
            if (connectionWaiters.size > 1) return // already connecting
        }
        billing.startConnection(object : BillingClientStateListener {
            override fun onBillingSetupFinished(result: BillingResult) {
                val waiters = synchronized(connectionWaiters) {
                    connectionWaiters.toList().also { connectionWaiters.clear() }
                }
                waiters.forEach { it(result) }
            }

            // The next call reconnects.
            override fun onBillingServiceDisconnected() {}
        })
    }

    /** Subscriptions and one-time products with these ids (a query takes one product type). */
    private fun queryDetails(call: PluginCall, ids: List<String>, done: (List<ProductDetails>) -> Unit) {
        val found = mutableListOf<ProductDetails>()
        fun query(types: List<String>) {
            val type = types.firstOrNull()
            if (type == null) {
                done(found)
                return
            }
            val products = ids.map {
                QueryProductDetailsParams.Product.newBuilder().setProductId(it).setProductType(type).build()
            }
            val params = QueryProductDetailsParams.newBuilder().setProductList(products).build()
            billing.queryProductDetailsAsync(params) { result, details ->
                val code = result.responseCode
                if (code != BillingResponseCode.OK && code != BillingResponseCode.ITEM_UNAVAILABLE) {
                    reject(call, result)
                    return@queryProductDetailsAsync
                }
                found += details.productDetailsList
                query(types.drop(1))
            }
        }
        query(listOf(ProductType.SUBS, ProductType.INAPP))
    }

    /** Subscriptions: the base plan (offer without offer id). One-time products: the first purchase option. */
    private fun defaultOfferToken(product: ProductDetails): String? =
        product.subscriptionOfferDetails?.let { offers -> (offers.firstOrNull { it.offerId == null } ?: offers.firstOrNull())?.offerToken }
            ?: product.oneTimePurchaseOfferDetailsList?.firstOrNull()?.offerToken

    private fun productJson(product: ProductDetails): JSObject? {
        val json = JSObject().apply {
            put("productId", product.productId)
            put("title", product.name)
            put("description", product.description)
        }
        val subscriptionOffers = product.subscriptionOfferDetails
        if (product.productType == ProductType.SUBS && subscriptionOffers != null) {
            val basePlan = subscriptionOffers.firstOrNull { it.offerId == null } ?: subscriptionOffers.firstOrNull()
            val recurring = basePlan?.pricingPhases?.pricingPhaseList?.lastOrNull() ?: return null
            json.put("kind", "subscription")
            json.put("price", priceJson(recurring.formattedPrice, recurring.priceAmountMicros, recurring.priceCurrencyCode))
            json.put("subscriptionPeriod", recurring.billingPeriod)
            json.put(
                "offers",
                JSArray(
                    subscriptionOffers.map { offer ->
                        JSObject().apply {
                            put("offerToken", offer.offerToken)
                            put("basePlanId", offer.basePlanId)
                            offer.offerId?.let { put("offerId", it) }
                            put("tags", JSArray(offer.offerTags))
                            put(
                                "pricingPhases",
                                JSArray(
                                    offer.pricingPhases.pricingPhaseList.map { phase ->
                                        JSObject().apply {
                                            put("price", priceJson(phase.formattedPrice, phase.priceAmountMicros, phase.priceCurrencyCode))
                                            put("billingPeriod", phase.billingPeriod)
                                            put("billingCycleCount", phase.billingCycleCount)
                                        }
                                    },
                                ),
                            )
                        }
                    },
                ),
            )
            return json
        }
        val options = product.oneTimePurchaseOfferDetailsList.orEmpty()
        val default = product.oneTimePurchaseOfferDetails ?: options.firstOrNull() ?: return null
        json.put("kind", "one_time")
        json.put("price", priceJson(default.formattedPrice, default.priceAmountMicros, default.priceCurrencyCode))
        json.put(
            "offers",
            JSArray(
                options.map { option ->
                    JSObject().apply {
                        put("offerToken", option.offerToken)
                        put("offerId", option.offerId)
                        put("tags", JSArray(option.offerTags))
                        put(
                            "pricingPhases",
                            JSArray(
                                listOf(
                                    JSObject().apply {
                                        put("price", priceJson(option.formattedPrice, option.priceAmountMicros, option.priceCurrencyCode))
                                        put("billingPeriod", "")
                                        put("billingCycleCount", 0)
                                    },
                                ),
                            ),
                        )
                    }
                },
            ),
        )
        return json
    }

    private fun purchaseJson(purchase: Purchase) = JSObject().apply {
        put("store", "play_store")
        put("purchaseToken", purchase.purchaseToken)
        put("productId", purchase.products.firstOrNull())
    }

    private fun reject(call: PluginCall, result: BillingResult) {
        val code = responseCodeName(result.responseCode)
        call.reject(result.debugMessage.ifEmpty { code }, code)
    }

    companion object {
        /** Micros → the currency's ISO 4217 minor unit, e.g. 4 990 000 EUR → 499, 480 000 000 JPY → 480. */
        fun amountMinor(micros: Long, currency: String): Long {
            val digits = Currency.getInstance(currency).defaultFractionDigits.coerceAtLeast(0)
            return BigDecimal.valueOf(micros).movePointLeft(6 - digits).setScale(0, RoundingMode.HALF_UP).toLong()
        }

        private fun priceJson(formatted: String, micros: Long, currency: String) = JSObject().apply {
            put("formatted", formatted)
            put("amountMinor", amountMinor(micros, currency))
            put("currency", currency)
        }

        private fun responseCodeName(code: Int): String = when (code) {
            BillingResponseCode.OK -> "OK"
            BillingResponseCode.USER_CANCELED -> "USER_CANCELED"
            BillingResponseCode.SERVICE_UNAVAILABLE -> "SERVICE_UNAVAILABLE"
            BillingResponseCode.SERVICE_DISCONNECTED -> "SERVICE_DISCONNECTED"
            BillingResponseCode.BILLING_UNAVAILABLE -> "BILLING_UNAVAILABLE"
            BillingResponseCode.ITEM_UNAVAILABLE -> "ITEM_UNAVAILABLE"
            BillingResponseCode.DEVELOPER_ERROR -> "DEVELOPER_ERROR"
            BillingResponseCode.ERROR -> "ERROR"
            BillingResponseCode.ITEM_ALREADY_OWNED -> "ITEM_ALREADY_OWNED"
            BillingResponseCode.ITEM_NOT_OWNED -> "ITEM_NOT_OWNED"
            BillingResponseCode.NETWORK_ERROR -> "NETWORK_ERROR"
            BillingResponseCode.FEATURE_NOT_SUPPORTED -> "FEATURE_NOT_SUPPORTED"
            else -> "BILLING_ERROR_$code"
        }
    }
}
