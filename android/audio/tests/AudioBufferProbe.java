import android.media.AudioFormat;
import android.media.AudioTrack;

/** Queries the real Android client buffer policy without starting playback or a GUI. */
public final class AudioBufferProbe {
    public static void main(String[] args) {
        double maximumMs = args.length == 0 ? Double.POSITIVE_INFINITY
                : Double.parseDouble(args[0]);
        for (int rate : new int[] {44100, 48000, 96000}) {
            int bytes = AudioTrack.getMinBufferSize(rate, AudioFormat.CHANNEL_OUT_STEREO,
                    AudioFormat.ENCODING_PCM_16BIT);
            if (bytes <= 0) throw new AssertionError("buffer query failed: " + bytes);
            double durationMs = 1000.0 * bytes / (4.0 * rate);
            System.out.printf("rate=%d bytes=%d duration_ms=%.3f%n", rate, bytes, durationMs);
            if (durationMs > maximumMs) {
                throw new AssertionError("device latency duplicated in client buffer: " + durationMs);
            }
        }
    }
}
