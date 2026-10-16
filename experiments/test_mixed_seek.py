"""Generation and converter cancellation races, without devices or listeners."""
from pathlib import Path
import tempfile
import threading
import time
import unittest
from unittest.mock import Mock

import navidrome as nav
from mixed_seek import MixedClient
from output_clock import Converter, mapping
from seek import validate_envelope
from test_output_clock import signal, reference


class MixedSeekTests(unittest.TestCase):
    def client(self):
        sink = Mock()
        client = MixedClient(None, sink, Path('unused'),
                             [{'bytes': 40000000, 'frames': 160 * 44100, 'sample_rate': 44100}], 48000)
        client.generation = 3
        client.plans[3] = {'segments': [], 'sample_rate': 44100}
        client.handles[3] = Mock()
        client.policy = Mock()
        return client, sink

    def test_late_native_completion_changes_neither_policy_nor_current_input(self):
        client, sink = self.client()
        self.assertFalse(client.admit_native(1, {'frames': 88201}, (100, 1, 1)))
        self.assertEqual(client.input_rejected, 1)
        self.assertEqual(client.plans[3]['segments'], [])
        client.policy.observe.assert_not_called()
        sink.append.assert_not_called()
        self.assertTrue(client.admit_native(3, {'frames': 88201}, (100, 1, 1)))
        client.policy.observe.assert_called_once_with(100, 1, 1)

    def test_converted_completion_is_checked_again_at_output_not_only_before_conversion(self):
        client, sink = self.client()
        with self.assertRaises(nav.Cancelled):
            client.admit_output(1, 0, bytes(8))
        self.assertEqual(client.stale_rejected, 1)
        sink.append.assert_not_called()
        client.handles[3].write.assert_not_called()
        sink.append.return_value = True
        client.admit_output(3, 0, bytes(8))
        sink.append.assert_called_once_with(3, 0, bytes(8))
        client.handles[3].write.assert_called_once_with(bytes(8))

    def test_converted_output_and_switch_use_one_admission_lock(self):
        client, sink = self.client()
        entered, release, switched = threading.Event(), threading.Event(), threading.Event()
        def append(*_):
            entered.set()
            self.assertTrue(release.wait(2))
            return True
        sink.append.side_effect = append
        worker = threading.Thread(target=client.admit_output, args=(3, 0, bytes(8)))
        def switch():
            with client.lock:
                client.generation = 4
                switched.set()
        worker.start()
        other = threading.Thread(target=switch)
        try:
            self.assertTrue(entered.wait(2))
            other.start()
            self.assertFalse(switched.wait(0.05))
            release.set()
            worker.join(2)
            other.join(2)
            self.assertTrue(switched.is_set())
            with self.assertRaises(nav.Cancelled):
                client.admit_output(3, 1, bytes(8))
            self.assertEqual(sink.append.call_count, 1)
        finally:
            release.set()
            worker.join(3)
            if other.ident:
                other.join(3)

    def test_native_envelope_checks_rate_and_partial_interval(self):
        envelope = {'generation': 4, 'track': 1, 'source_start': 197, 'frames': 123,
            'metadata': {'quality': 'opus-64', 'sample_rate': 96000, 'channels': 2,
                'presentation': {'frames': 123}, 'discard_start_frames': 11520,
                'discard_end_frames': 11521, 'decoded_frames': 23164,
                'encoded_interval': {'frames': 23163}, 'codec_sample_rate': 48000,
                'codec_decoded_frames': 11582, 'resample_tail_frames': 1}}
        validate_envelope(envelope, 4, 1, 197, 'opus-64', frames=123, rate=96000)
        for frames, rate in ((124, 96000), (123, 48000)):
            with self.assertRaises(RuntimeError):
                validate_envelope(envelope, 4, 1, 197, 'opus-64', frames=frames, rate=rate)

    def test_cancel_during_a_real_pipe_send_discards_buffered_tail_for_all_clocks(self):
        for source, output in ((44100, 48000), (96000, 44100), (44100, 96000), (48000, 48000)):
            with self.subTest(source=source, output=output), tempfile.TemporaryDirectory() as temp:
                part = mapping([{'sample_rate': source, 'frames': source * 4}],
                               [{'origin': 0, 'frames': source * 3}], output)[0]
                cancel, chunks = threading.Event(), []
                def send(_, pcm):
                    chunks.append(pcm)
                    cancel.set()  # Model switch during an accepted callback.
                converter = Converter(part, output, send, Path(temp), cancel=cancel)
                try:
                    pcm = signal(source, part['input_end'])
                    with self.assertRaises(nav.Cancelled):
                        converter.append(0, pcm)
                        converter.finish()
                    before = len(chunks)
                    with self.assertRaises(nav.Cancelled):
                        converter.finish()
                    self.assertEqual(len(chunks), before)
                    self.assertEqual(before, 1)
                finally:
                    converter.close()
                if converter.process:
                    self.assertIsNotNone(converter.process.poll())

    def test_cancelled_eof_drain_never_emits_the_pending_tail(self):
        with tempfile.TemporaryDirectory() as temp:
            source, output = 44100, 48000
            part = mapping([{'sample_rate': source, 'frames': source * 2 + 1}],
                           [{'origin': 0, 'frames': source * 2 + 1}], output)[0]
            chunks, cancel = [], threading.Event()
            converter = Converter(part, output, lambda _, pcm: chunks.append(pcm), Path(temp), cancel=cancel)
            try:
                converter.append(0, signal(source, part['input_end']))
                self.assertLess(converter.output_frames, part['output_frames'])
                count = len(chunks)
                cancel.set()
                with self.assertRaises(nav.Cancelled):
                    converter.finish()
                self.assertEqual(len(chunks), count)
                self.assertFalse(converter.finished)
            finally:
                converter.close()
            self.assertIsNotNone(converter.process.poll())

    def test_switch_during_real_eof_drain_stops_remaining_output(self):
        with tempfile.TemporaryDirectory() as temp:
            part = mapping([{'sample_rate': 44100, 'frames': 88201}],
                           [{'origin': 0, 'frames': 88201}], 48000)[0]
            cancel, draining = threading.Event(), threading.Event()
            sent = []
            def send(_, pcm):
                sent.append(pcm)
                if draining.is_set():
                    cancel.set()
            converter = Converter(part, 48000, send, Path(temp), cancel=cancel)
            try:
                converter.append(0, signal(44100, part['input_end']))
                before = len(sent)
                draining.set()
                with self.assertRaises(nav.Cancelled):
                    converter.finish()
                self.assertEqual(len(sent), before + 1)
                self.assertFalse(converter.finished)
            finally:
                converter.close()
            self.assertIsNotNone(converter.process.poll())

    def test_cancel_interrupts_wait_for_a_stalled_converter(self):
        with tempfile.TemporaryDirectory() as temp:
            part = mapping([{'sample_rate': 44100, 'frames': 100000}],
                           [{'origin': 0, 'frames': 88201}], 48000)[0]
            cancel, errors = threading.Event(), []
            converter = Converter(part, 48000, lambda *_: self.fail('unexpected output'), Path(temp), cancel=cancel)
            def pump():
                try:
                    # Live FFmpeg is waiting for input; force the other side to
                    # wait for output so cancellation must interrupt select().
                    converter._pump(memoryview(b''), 1)
                except BaseException as error:
                    errors.append(error)
                finally:
                    converter.close()
            worker = threading.Thread(target=pump)
            worker.start()
            try:
                time.sleep(0.05)
                began = time.monotonic()
                cancel.set()
                worker.join(2)
                self.assertFalse(worker.is_alive())
                self.assertLess(time.monotonic() - began, 1)
                self.assertEqual(len(errors), 1)
                self.assertIsInstance(errors[0], nav.Cancelled)
                self.assertIsNotNone(converter.process.poll())
            finally:
                cancel.set()
                worker.join(3)

    def test_fresh_generation_resumes_at_odd_source_position_without_old_history(self):
        for source, output in ((44100, 48000), (96000, 48000), (48000, 44100), (44100, 96000)):
            with self.subTest(source=source, output=output), tempfile.TemporaryDirectory() as temp:
                full = signal(source, 4 * source + 317)
                part = mapping([{'sample_rate': source, 'frames': len(full) // 8}],
                               [{'origin': source + 151, 'frames': source + 9}], output)[0]
                chunks = []
                converter = Converter(part, output, lambda _, pcm: chunks.append(pcm), Path(temp))
                try:
                    converter.append(part['input_origin'], full[part['input_origin'] * 8:part['input_end'] * 8])
                    converter.finish()
                finally:
                    converter.close()
                start = part['origin'] * output // source
                self.assertEqual(b''.join(chunks), reference(full, source, output)[start * 8:(start + part['output_frames']) * 8])


if __name__ == '__main__':
    unittest.main()
