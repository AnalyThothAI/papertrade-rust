//! Hyperliquid best bid/offer for one perp, in Papertrade's wire price scale.
//!
//! Papertrade executes at the midpoint of this same book (read on-chain through
//! the BBO precompile), so the mid here is what an open or close would get.

use std::time::{Duration, Instant};

use futures::StreamExt;
use hypersdk::Decimal;
use hypersdk::hypercore::{self, Incoming, Subscription, ws::Event};
use rust_decimal::prelude::ToPrimitive;
use tokio::sync::watch;

#[derive(Debug, Clone, Copy)]
pub struct Quote {
    mid: u64,
    at: Instant,
}

impl Quote {
    /// A mid in wire scale, observed now.
    pub fn new(mid: u64) -> Self {
        Self { mid, at: Instant::now() }
    }

    /// The mid in wire scale, or `None` once the quote is too old to act on.
    pub fn fresh_mid(self, max_age: Duration) -> Option<u64> {
        (self.at.elapsed() <= max_age).then_some(self.mid)
    }
}

/// Keeps `quote` at the latest mid, forever. The SDK reconnects and resubscribes on its own.
pub async fn follow_price(coin: String, price_decimals: u32, quote: watch::Sender<Option<Quote>>) {
    let mut ws = hypercore::mainnet_ws();
    ws.subscribe(Subscription::Bbo { coin });
    while let Some(event) = ws.next().await {
        match event {
            Event::Message(Incoming::Bbo(bbo)) => {
                let (Some(bid), Some(ask)) = (bbo.bid(), bbo.ask()) else { continue };
                if let Some(mid) = mid_raw(bid.px, ask.px, price_decimals) {
                    quote.send_replace(Some(Quote::new(mid)));
                }
            }
            Event::Disconnected => {
                quote.send_replace(None);
            }
            _ => {}
        }
    }
}

/// Same integer midpoint the Exchange contract computes: `(bid + ask) / 2`.
pub fn mid_raw(bid: Decimal, ask: Decimal, price_decimals: u32) -> Option<u64> {
    let scale = Decimal::from(10u64.checked_pow(price_decimals)?);
    let wire = |price: Decimal| price.checked_mul(scale)?.trunc().to_u64();
    Some((wire(bid)? + wire(ask)?) / 2)
}

/// A wire price is the USD price times a power of ten; returns that power.
pub fn price_decimals(price_scale: u64) -> Option<u32> {
    let decimals = price_scale.checked_ilog10()?;
    (10u64.pow(decimals) == price_scale).then_some(decimals)
}

/// A wire price as a decimal USD string.
pub fn format_price(raw: u64, price_decimals: u32) -> String {
    Decimal::new(raw as i64, price_decimals).to_string()
}

#[cfg(test)]
mod tests {
    use hypersdk::dec;

    use super::*;

    #[test]
    fn quotes_convert_to_the_contracts_integer_midpoint() {
        // Production values: the BBO precompile returned 827930 / 827940 for this book.
        assert_eq!(mid_raw(dec!(82793.0), dec!(82794.0), 1), Some(827_935));
        assert_eq!(mid_raw(dec!(2495.0), dec!(2495.1), 2), Some(249_505));
    }

    #[test]
    fn a_half_unit_truncates_like_the_contracts_integer_division() {
        assert_eq!(mid_raw(dec!(82793.0), dec!(82793.1), 1), Some(827_930));
    }

    #[test]
    fn the_price_scale_must_be_a_power_of_ten() {
        assert_eq!(price_decimals(10), Some(1));
        assert_eq!(price_decimals(100), Some(2));
        assert_eq!(price_decimals(1), Some(0));
        assert_eq!(price_decimals(25), None);
        assert_eq!(price_decimals(0), None);
    }

    #[test]
    fn wire_prices_format_as_usd() {
        assert_eq!(format_price(827_935, 1), "82793.5");
        assert_eq!(format_price(249_505, 2), "2495.05");
    }
}
