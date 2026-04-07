export MINIMAX_API_KEY="sk-cp-ILIiTE1ZG_1Zsk0vqbbgM3OPzcuccYJI5w_XQNluX7X9b92mMNgeA5SB4ToqBmXSpgducAb709sjR0H3ua6Yibo2EJwncVHi8OgKHSRjZxfo1ukdfLHNj0k"

curl -s https://api.minimaxi.com/anthropic/v1/models \
     -H "x-api-key: $MINIMAX_API_KEY" \
     -H "anthropic-version: 2023-06-01"
