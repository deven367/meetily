-- Add openRouterApiKey column to transcript_settings for cloud transcription via OpenRouter
ALTER TABLE transcript_settings ADD COLUMN openRouterApiKey TEXT;
