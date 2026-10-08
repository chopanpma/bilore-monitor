"""Glue for bilore-monitor's DecisionSpec specs (specs/*.spec).

Every scenario delegates to the exact cargo test that pins it (the spec's
`# delegates:` comment): `cargo test <target> <test> -- --exact`. A renamed or
deleted Rust test fails loudly: --exact then matches nothing, cargo exits 0
with '0 passed', and the '1 passed' check below turns that into a failure.

Scenarios of `proposed` decisions have no Rust test yet; their glue raises
NotImplementedError so the gate lists them as the work queue.
"""

import os
import subprocess

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
CARGO = os.environ.get("CARGO", "cargo")


def _delegate(test_name, target=("--lib",)):
    cmd = [CARGO, "test", *target, test_name, "--", "--exact"]
    proc = subprocess.run(cmd, cwd=ROOT, capture_output=True, text=True)
    output = proc.stdout + proc.stderr
    assert proc.returncode == 0, "delegated test failed: %s\n%s" % (" ".join(cmd), output)
    assert "1 passed" in output, (
        "delegation ran zero tests (renamed Rust test?): %s\n%s" % (" ".join(cmd), output)
    )


def _unpinned(scenario):
    raise NotImplementedError("no cargo test pins this yet: " + scenario)


# --- MON-001 ---------------------------------------------------------------

def day_rollover_retrains_and_keeps_old_model_on_failure():
    _unpinned('day_rollover_retrains_and_keeps_old_model_on_failure')


# --- MON-002 ---------------------------------------------------------------

def a_won_trade_frees_the_slot_for_the_next_setup():
    _delegate('tests::a_won_trade_frees_the_slot_for_the_next_setup', target=['--bin', 'bilore-monitor'])


def lost_and_expired_trades_free_the_slot_too():
    _delegate('tests::lost_and_expired_trades_free_the_slot_too', target=['--bin', 'bilore-monitor'])


def an_open_trade_keeps_the_slot_busy():
    _delegate('tests::an_open_trade_keeps_the_slot_busy', target=['--bin', 'bilore-monitor'])


def an_empty_slot_is_armed_and_has_nothing_to_take():
    _delegate('tests::an_empty_slot_is_armed_and_has_nothing_to_take', target=['--bin', 'bilore-monitor'])


def a_direction_flip_hands_back_the_invalidated_trade_and_re_arms_the_slot():
    _delegate('tests::a_direction_flip_hands_back_the_invalidated_trade_and_re_arms_the_slot', target=['--bin', 'bilore-monitor'])


def an_entered_trade_keeps_the_slot_locked_on_a_flip():
    _delegate('tests::an_entered_trade_keeps_the_slot_locked_on_a_flip', target=['--bin', 'bilore-monitor'])


def the_same_direction_leaves_the_slot_untouched():
    _delegate('tests::the_same_direction_leaves_the_slot_untouched', target=['--bin', 'bilore-monitor'])


def an_empty_slot_has_nothing_to_invalidate():
    _delegate('tests::an_empty_slot_has_nothing_to_invalidate', target=['--bin', 'bilore-monitor'])


def flip_buffer_ticks_defaults_to_4_and_rejects_bad_values():
    _delegate('tests::flip_buffer_ticks_defaults_to_4_and_rejects_bad_values', target=['--bin', 'bilore-monitor'])


# --- MON-003 ---------------------------------------------------------------

def a_stopped_out_trade_blocks_the_same_direction_at_the_same_entry():
    _delegate('tests::a_stopped_out_trade_blocks_the_same_direction_at_the_same_entry', target=['--bin', 'bilore-monitor'])


def a_winning_or_expired_trade_blocks_nothing():
    _delegate('tests::a_winning_or_expired_trade_blocks_nothing', target=['--bin', 'bilore-monitor'])


def a_level_stopped_out_this_session_is_not_retaken_in_that_direction():
    _delegate('globex::tests::a_level_stopped_out_this_session_is_not_retaken_in_that_direction')


def a_stopped_level_is_not_retaken_in_the_same_direction():
    _delegate('rolling::tests::a_stopped_level_is_not_retaken_in_the_same_direction')


def stopped_levels_restored_from_the_db_block_after_a_restart():
    _delegate('tests::stopped_levels_restored_from_the_db_block_after_a_restart', target=['--bin', 'bilore-monitor'])


# --- MON-004 ---------------------------------------------------------------

def only_the_newest_open_trade_per_strategy_is_restored():
    _delegate('tests::only_the_newest_open_trade_per_strategy_is_restored', target=['--bin', 'bilore-monitor'])


def a_pending_row_restores_as_pending_still_waiting_for_its_fill():
    _delegate('tests::a_pending_row_restores_as_pending_still_waiting_for_its_fill', target=['--bin', 'bilore-monitor'])


def an_entered_row_restores_as_an_entered_trade_with_its_fill():
    _delegate('tests::an_entered_row_restores_as_an_entered_trade_with_its_fill', target=['--bin', 'bilore-monitor'])


def resolved_or_malformed_rows_are_not_restored():
    _delegate('tests::resolved_or_malformed_rows_are_not_restored', target=['--bin', 'bilore-monitor'])


def replayed_ticks_never_generate_setups():
    _delegate('tests::replayed_ticks_never_generate_setups', target=['--bin', 'bilore-monitor'])


def a_trade_ignores_ticks_from_before_it_existed():
    _delegate('tests::a_trade_ignores_ticks_from_before_it_existed', target=['--bin', 'bilore-monitor'])


# --- MON-005 ---------------------------------------------------------------

def a_fully_recorded_session_gives_its_poc_and_value_area():
    _delegate('levels::tests::a_fully_recorded_session_gives_its_poc_and_value_area')


def a_partly_recorded_session_is_not_trusted():
    _delegate('levels::tests::a_partly_recorded_session_is_not_trusted')


def no_ticks_no_levels():
    _delegate('levels::tests::no_ticks_no_levels')


# --- MON-006 ---------------------------------------------------------------

def above_the_poc_it_sells_at_the_vah_with_a_60_tick_stop_and_2r_target():
    _delegate('globex::tests::above_the_poc_it_sells_at_the_vah_with_a_60_tick_stop_and_2r_target')


def below_the_poc_it_buys_at_the_val():
    _delegate('globex::tests::below_the_poc_it_buys_at_the_val')


def far_above_every_level_there_is_no_setup():
    _delegate('globex::tests::far_above_every_level_there_is_no_setup')


def globex_hours_map_to_the_next_trading_day():
    _delegate('globex::tests::globex_hours_map_to_the_next_trading_day')


# --- MON-007 ---------------------------------------------------------------

def windows_split_the_day_at_17_02_08_30_and_15():
    _delegate('rolling::tests::windows_split_the_day_at_17_02_08_30_and_15')


def the_three_strategies_use_16_periods_and_their_geometry():
    _delegate('rolling::tests::the_three_strategies_use_16_periods_and_their_geometry')


def evening_trades_are_stored_under_the_next_trading_day():
    _delegate('rolling::tests::evening_trades_are_stored_under_the_next_trading_day')


def only_completed_periods_count():
    _delegate('rolling::tests::only_completed_periods_count')


def the_snapshot_merges_the_last_n_periods_with_their_high_and_low():
    _delegate('rolling::tests::the_snapshot_merges_the_last_n_periods_with_their_high_and_low')


def a_gap_of_three_hours_or_more_resets_the_window():
    _delegate('rolling::tests::a_gap_of_three_hours_or_more_resets_the_window')


def above_the_rolling_poc_it_sells_at_the_nearest_level_above():
    _delegate('rolling::tests::above_the_rolling_poc_it_sells_at_the_nearest_level_above')


def above_the_value_area_the_rolling_high_is_the_entry():
    _delegate('rolling::tests::above_the_value_area_the_rolling_high_is_the_entry')


# --- MON-008 ---------------------------------------------------------------

def due_right_at_the_rth_close_on_a_weekday():
    _delegate('daily_summary::tests::due_right_at_the_rth_close_on_a_weekday')


def not_due_before_the_close():
    _delegate('daily_summary::tests::not_due_before_the_close')


def not_due_after_the_one_hour_window():
    _delegate('daily_summary::tests::not_due_after_the_one_hour_window')


def not_due_on_weekends():
    _delegate('daily_summary::tests::not_due_on_weekends')


def not_due_twice_the_same_day():
    _delegate('daily_summary::tests::not_due_twice_the_same_day')


def uses_chicago_wall_clock_after_dst_ends():
    _delegate('daily_summary::tests::uses_chicago_wall_clock_after_dst_ends')


def today_line_counts_only_todays_rows():
    _delegate('daily_summary::tests::today_line_counts_only_todays_rows')


def week_line_covers_monday_through_today_and_excludes_last_week():
    _delegate('daily_summary::tests::week_line_covers_monday_through_today_and_excludes_last_week')


def total_line_shows_the_cumulative_gate():
    _delegate('daily_summary::tests::total_line_shows_the_cumulative_gate')


def a_strategy_with_no_trades_says_so_for_today_and_week():
    _delegate('daily_summary::tests::a_strategy_with_no_trades_says_so_for_today_and_week')


# --- MON-009 ---------------------------------------------------------------

def mes_contract_has_1_25_tick_value():
    _delegate('economics::tests::mes_contract_has_1_25_tick_value')


def mnq_contract_has_the_distinct_0_50_tick_value():
    _delegate('economics::tests::mnq_contract_has_the_distinct_0_50_tick_value')


def mgc_contract_has_its_own_tick_size_and_value():
    _delegate('economics::tests::mgc_contract_has_its_own_tick_size_and_value')


def unrecognized_root_falls_back_to_mes_economics():
    _delegate('economics::tests::unrecognized_root_falls_back_to_mes_economics')


# --- MON-010 ---------------------------------------------------------------

def result_message_shows_won_outcome_and_pnl():
    _delegate('telegram::tests::result_message_shows_won_outcome_and_pnl')


def no_sound_here_overlaps_v1s_set():
    _delegate('sound::tests::no_sound_here_overlaps_v1s_set')


def every_alert_kind_maps_to_a_distinct_primary_sound():
    _delegate('sound::tests::every_alert_kind_maps_to_a_distinct_primary_sound')


# --- MON-011 (value-area alert, 2026-10-07) ---------------------------------


def session_of_maps_rth_globex_and_the_gap():
    _delegate('va_alert::tests::session_of_maps_rth_globex_and_the_gap')


def the_profile_restarts_at_each_session_start():
    _delegate('va_alert::tests::the_profile_restarts_at_each_session_start')


def a_bell_shaped_profile_is_d():
    _delegate('va_alert::tests::a_bell_shaped_profile_is_d')


def a_profile_with_its_poc_in_the_upper_third_is_p():
    _delegate('va_alert::tests::a_profile_with_its_poc_in_the_upper_third_is_p')


def a_profile_with_its_poc_in_the_lower_third_is_b():
    _delegate('va_alert::tests::a_profile_with_its_poc_in_the_lower_third_is_b')


def a_double_distribution_is_irregular():
    _delegate('va_alert::tests::a_double_distribution_is_irregular')


def a_thin_elongated_profile_is_irregular():
    _delegate('va_alert::tests::a_thin_elongated_profile_is_irregular')


def too_early_in_the_session_is_not_classified():
    _delegate('va_alert::tests::too_early_in_the_session_is_not_classified')


def price_above_vah_fades_short():
    _delegate('va_alert::tests::price_above_vah_fades_short')


def price_below_val_fades_long():
    _delegate('va_alert::tests::price_below_val_fades_long')


def price_inside_the_value_area_never_alerts():
    _delegate('va_alert::tests::price_inside_the_value_area_never_alerts')


def an_irregular_profile_never_alerts():
    _delegate('va_alert::tests::an_irregular_profile_never_alerts')


def two_of_three_agreeing_with_the_fade_alerts():
    _delegate('va_alert::tests::two_of_three_agreeing_with_the_fade_alerts')


def one_of_three_does_not_alert():
    _delegate('va_alert::tests::one_of_three_does_not_alert')


def neutral_factors_do_not_count():
    _delegate('va_alert::tests::neutral_factors_do_not_count')


def one_alert_per_period():
    _delegate('va_alert::tests::one_alert_per_period')


def a_poc_cross_re_arms_within_the_period():
    _delegate('va_alert::tests::a_poc_cross_re_arms_within_the_period')


def a_new_period_re_arms():
    _delegate('va_alert::tests::a_new_period_re_arms')


def the_text_profile_marks_vah_poc_val_and_price():
    _delegate('va_alert::tests::the_text_profile_marks_vah_poc_val_and_price')


def the_text_profile_fits_in_24_rows():
    _delegate('va_alert::tests::the_text_profile_fits_in_24_rows')


def va_alert_message_shows_alignment_and_profile():
    _delegate('telegram::tests::va_alert_message_shows_alignment_and_profile')


def result_message_shows_lost_outcome():
    _delegate('telegram::tests::result_message_shows_lost_outcome')


# --- MON-012 (Telegram policy) -------------------------------------------------


def only_won_and_lost_results_go_to_telegram():
    _delegate('tests::only_won_and_lost_results_go_to_telegram', target=('--bin', 'bilore-monitor'))


# --- MON-013 (only the value-area alert makes a sound) ---------------------------

def only_the_value_area_alert_makes_a_sound():
    _delegate('sound::tests::only_the_value_area_alert_makes_a_sound')


def the_value_area_sound_is_distinct_from_v1_and_the_cockpit():
    _delegate('sound::tests::the_value_area_sound_is_distinct_from_v1_and_the_cockpit')
