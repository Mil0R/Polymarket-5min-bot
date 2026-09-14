import unittest
from digital_fair_value_study import partition, fit, predict, score, frozen_probability_gate


class StudyTests(unittest.TestCase):
    def test_fixed_boundary_is_half_open_and_independent_of_sample_count(self):
        config = dict(start='2026-01-01T00:00:00Z', train_end='2026-01-02T00:00:00Z',
                      validation_end='2026-01-03T00:00:00Z', end='2026-01-04T00:00:00Z')
        from digital_fair_value_study import timestamp
        t = timestamp(config['train_end'])
        self.assertEqual(partition(t - 1, config), 'train')
        self.assertEqual(partition(t, config), 'validation')
        self.assertIsNone(partition(timestamp(config['end']), config))

    def test_review_outcomes_do_not_change_fitted_model(self):
        def row(split, y):
            return dict(asset='btc', checkpoint=30, split=split, y=y, p=.7,
                        observed_vol=.1, floor=.8, remaining=30, margin=1,
                        opening=100., current=100., final=101. if y else 99.)
        rows = [row('train', i % 2) for i in range(60)] + [row('validation', i % 2) for i in range(20)]
        self.assertEqual(fit(rows+[row('historical_review',0)]), fit(rows+[row('historical_review',1)]))

    def test_empirical_direction_and_smoothing(self):
        model = dict(kind='empirical', residuals_bps=[-2, 0, 2])
        self.assertGreater(predict(dict(opening=99,current=100),model), .5)
        self.assertLess(predict(dict(opening=101,current=100),model), .5)
        self.assertGreater(predict(dict(opening=101,current=100),model), 0)

    def test_both_sides_have_same_proper_score(self):
        pairs = [(.9,1),(.1,1),(.2,0)]
        self.assertAlmostEqual(score(pairs)['brier'],score([(1-p,1-y) for p,y in pairs])['brier'])

    def test_frozen_partition_has_no_training_split(self):
        config = dict(frozen_test=True, start='2026-01-01T00:00:00Z', end='2026-01-02T00:00:00Z')
        from digital_fair_value_study import timestamp
        self.assertEqual(partition(timestamp(config['start']), config), 'frozen_test')
        self.assertIsNone(partition(timestamp(config['end']), config))

    def test_probability_gate_requires_data_and_market_advantage(self):
        s = {'samples':80,'wins':40,'mean_probability':.5,'win_rate':.5,'brier':.1,'log_loss':.3}
        market = dict(s,brier=.2,log_loss=.4)
        current = dict(s,brier=.15,log_loss=.35)
        prefix='frozen_test/btc/30s/'
        result={'scores':{prefix+'selected/paired_model':s,prefix+'selected/paired_market':market,
                          prefix+'current/paired_model':current},
                'paired_brier_delta_by_day':{prefix+'selected':{str(i):{} for i in range(5)}}}
        models={'btc/30s':{'status':'fitted'}}
        policy={'min_paired_samples_per_asset':75,'min_utc_days':5}
        self.assertEqual(frozen_probability_gate(result,models,policy)['status'],'PASSED')


if __name__ == '__main__':
    unittest.main()
