//! Unit tests for `MouseButtonSpy` mask decoding and event parsing.

use super::MouseButtonMask;
use super::event::MouseButtonSpyEvent;
use crate::feature::DecodeEvent;

#[test]
fn pressed_reports_bit_zero_as_button_one() {
    let mask = MouseButtonMask::from_bits(0x0001);

    let pressed: Vec<u8> = mask.pressed().map(super::MouseButtonIndex::get).collect();

    assert_eq!(pressed, [1]);
}

#[test]
fn pressed_is_ascending_and_one_based() {
    let mask = MouseButtonMask::from_bits(0x0410); // bits 4 and 10 set

    let pressed: Vec<u8> = mask.pressed().map(super::MouseButtonIndex::get).collect();

    assert_eq!(pressed, [5, 11]);
}

#[test]
fn pressed_reports_the_highest_button() {
    let mask = MouseButtonMask::from_bits(0x8000); // bit 15 set

    let pressed: Vec<u8> = mask.pressed().map(super::MouseButtonIndex::get).collect();

    assert_eq!(pressed, [16]);
}

#[test]
fn pressed_is_empty_for_a_zero_mask() {
    let mask = MouseButtonMask::from_bits(0);

    assert_eq!(mask.pressed().count(), 0);
}

#[test]
fn from_bits_round_trips_through_bits() {
    let mask = MouseButtonMask::from_bits(0x1234);

    assert_eq!(mask.bits(), 0x1234);
}

#[test]
fn decodes_the_button_state_event_as_big_endian() {
    let mut payload = [0u8; 16];
    payload[0] = 0x04;
    payload[1] = 0x10;

    let event = MouseButtonSpyEvent::decode(0, &payload).unwrap();

    assert_eq!(
        event,
        MouseButtonSpyEvent::Buttons(MouseButtonMask::from_bits(0x0410))
    );
}

#[test]
fn decode_rejects_an_unknown_sub_id() {
    let payload = [0u8; 16];

    assert_eq!(MouseButtonSpyEvent::decode(1, &payload), None);
}
