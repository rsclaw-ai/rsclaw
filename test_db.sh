export ARK_API_KEY=1c70c010-af64-48a0-8d28-551da5b8e5f3
curl https://ark.cn-beijing.volces.com/api/v3/responses \
-H "Authorization: Bearer $ARK_API_KEY" \
-H 'Content-Type: application/json' \
-d '{
    "model": "doubao-seed-2-0-pro-260215",
    "input": [
        {
            "role": "user",
            "content": [
                {
                    "type": "input_image",
                    "image_url": "https://ark-project.tos-cn-beijing.volces.com/doc_image/ark_demo_img_1.png"
                },
                {
                    "type": "input_text",
                    "text": "你看见了什么？"
                }
            ]
        }
    ]
}'
