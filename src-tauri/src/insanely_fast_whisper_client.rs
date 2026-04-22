use anyhow::Result;
use hound::{WavSpec, WavWriter};
use log::debug;
use serde::Deserialize;
use std::io::Cursor;

#[derive(Deserialize)]
struct TranscriptOutput {
    text: Option<String>,
}

fn encode_samples_to_wav(samples: &[f32]) -> Result<Vec<u8>> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut buffer = Vec::new();
    {
        let cursor = Cursor::new(&mut buffer);
        let mut writer = WavWriter::new(cursor, spec)?;
        for sample in samples {
            let sample_i16 = (sample * i16::MAX as f32) as i16;
            writer.write_sample(sample_i16)?;
        }
        writer.finalize()?;
    }
    Ok(buffer)
}

pub fn transcribe_audio(samples: &[f32], model_name: &str, language: &str) -> Result<String> {
    let wav_bytes = encode_samples_to_wav(samples)?;

    // Write to a temporary WAV file
    let temp_dir = std::env::temp_dir();
    let wav_path = temp_dir.join("parler_ifw_input.wav");
    let json_path = temp_dir.join("parler_ifw_output.json");

    std::fs::write(&wav_path, &wav_bytes)?;

    // Build the command
    let mut cmd = std::process::Command::new("insanely-fast-whisper");
    cmd.arg("--file-name")
        .arg(&wav_path)
        .arg("--model-name")
        .arg(model_name)
        .arg("--transcript-path")
        .arg(&json_path);

    if language != "auto" {
        // Normalize zh variants to "zh" for Whisper
        let lang = if language == "zh-Hans" || language == "zh-Hant" {
            "zh"
        } else {
            language
        };
        cmd.arg("--language").arg(lang);
    }

    debug!("Running insanely-fast-whisper with model: {}", model_name);

    let output = cmd.output().map_err(|e| {
        anyhow::anyhow!(
            "Failed to run insanely-fast-whisper: {}. Make sure it is installed with: pip install insanely-fast-whisper",
            e
        )
    })?;

    // Clean up the WAV file regardless of success
    let _ = std::fs::remove_file(&wav_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = std::fs::remove_file(&json_path);
        return Err(anyhow::anyhow!(
            "insanely-fast-whisper failed: {}",
            stderr.trim()
        ));
    }

    // Parse the JSON output
    let json_content = std::fs::read_to_string(&json_path).map_err(|e| {
        anyhow::anyhow!("Failed to read insanely-fast-whisper output: {}", e)
    })?;
    let _ = std::fs::remove_file(&json_path);

    let transcript: TranscriptOutput = serde_json::from_str(&json_content).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse insanely-fast-whisper output: {}. Raw: {}",
            e,
            json_content
        )
    })?;

    Ok(transcript.text.unwrap_or_default().trim().to_string())
}
