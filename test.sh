curl -s https://api.minimaxi.com/anthropic/v1/messages \
     -H "Content-Type: application/json" \
     -H "x-api-key: sk-cp-ILIiTE1ZG_1Zsk0vqbbgM3OPzcuccYJI5w_XQNluX7X9b92mMNgeA5SB4ToqBmXSpgducAb709sjR0H3ua6Yibo2EJwncVHi8OgKHSRjZxfo1ukdfLHNj0k" \
     -H "anthropic-version: 2023-06-01" \
     -d '{
       "model": "claude-3-5-sonnet-20240620",
       "max_tokens": 1024,
       "messages": [
         {"role": "user", "content": "Hello, are you working via MiniMax?"}

       ]
     }'
