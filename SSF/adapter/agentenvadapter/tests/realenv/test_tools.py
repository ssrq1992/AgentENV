"""Offline helper checks only. These tests do NOT instantiate a VM/API/CSI mock."""
import hashlib
import unittest
from suite import operation_id, percentiles, redact, require


class ToolTests(unittest.TestCase):
    def test_operation_id_uses_tenant_and_null_separator(self):
        self.assertEqual(operation_id('a','b'), hashlib.sha256(b'a\0b').hexdigest())
        self.assertNotEqual(operation_id('a','bc'), operation_id('ab','c'))

    def test_nested_tokens_redacted_without_dropping_identity(self):
        v = redact({'instance':'i', 'sandbox':{'envdAccessToken':'s'}, 'x':[{'password':'p'}]})
        self.assertEqual(v['instance'],'i')
        self.assertEqual(v['sandbox']['envdAccessToken'],'[REDACTED]')
        self.assertEqual(v['x'][0]['password'],'[REDACTED]')

    def test_percentiles_keep_warmup_readonly_and_kind_separate(self):
        rows = [{'case':'performance','kind':'cold','readonly':False,'path':'measured-reuse','overall_ms':n} for n in range(1,101)]
        rows += [{'case':'performance','kind':'cold','readonly':False,'path':'warmup-initialize','overall_ms':10000},
                 {'case':'performance','kind':'template','readonly':True,'path':'measured-reuse','overall_ms':1}]
        r = percentiles(rows)
        self.assertEqual(len(r),3)
        self.assertEqual(r['performance/cold/rw/measured-reuse']['overall_ms'],{'n':100,'p50':50,'p95':95,'p99':99})

    def test_assertions_do_not_depend_on_python_optimization(self):
        with self.assertRaises(AssertionError): require(False,'bad')


if __name__ == '__main__':
    unittest.main()
