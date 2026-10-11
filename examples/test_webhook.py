"""
示例 webhook 数据和测试客户端
"""
import json

# 示例 webhook payload（来自 health-connect-webhook 文档）
EXAMPLE_PAYLOAD = {
    "timestamp": "2024-01-10T14:00:00.123Z",
    "app_version": "1.0.0",
    "steps": [
        {
            "count": 8421,
            "start_time": "2024-01-10T00:00:00Z",
            "end_time": "2024-01-10T23:59:59Z"
        }
    ],
    "heart_rate": [
        {
            "bpm": 72,
            "time": "2024-01-10T08:15:00Z"
        },
        {
            "bpm": 75,
            "time": "2024-01-10T08:30:00Z"
        }
    ],
    "sleep": [
        {
            "session_end_time": "2024-01-10T07:00:00Z",
            "duration_seconds": 28800,
            "stages": [
                {
                    "stage": "DEEP",
                    "start_time": "2024-01-09T23:00:00Z",
                    "end_time": "2024-01-10T01:00:00Z",
                    "duration_seconds": 7200
                },
                {
                    "stage": "LIGHT",
                    "start_time": "2024-01-10T01:00:00Z",
                    "end_time": "2024-01-10T05:00:00Z",
                    "duration_seconds": 14400
                },
                {
                    "stage": "REM",
                    "start_time": "2024-01-10T05:00:00Z",
                    "end_time": "2024-01-10T07:00:00Z",
                    "duration_seconds": 7200
                }
            ]
        }
    ],
    "distance": [
        {
            "meters": 6500,
            "start_time": "2024-01-10T00:00:00Z",
            "end_time": "2024-01-10T23:59:59Z"
        }
    ],
    "active_calories": [
        {
            "calories": 450,
            "start_time": "2024-01-10T00:00:00Z",
            "end_time": "2024-01-10T23:59:59Z"
        }
    ]
}


def send_test_webhook(url: str = "http://localhost:5700/webhook"):
    """发送测试 webhook 请求"""
    import requests
    
    response = requests.post(
        url,
        json=EXAMPLE_PAYLOAD,
        headers={"Content-Type": "application/json"}
    )
    
    print(f"Status Code: {response.status_code}")
    print(f"Response: {response.json()}")
    
    return response


if __name__ == "__main__":
    import sys
    
    url = sys.argv[1] if len(sys.argv) > 1 else "http://localhost:5700/webhook"
    
    print(f"Sending test webhook to {url}")
    print(f"Payload:\n{json.dumps(EXAMPLE_PAYLOAD, indent=2)}\n")
    
    try:
        send_test_webhook(url)
    except Exception as e:
        print(f"Error: {e}")
        sys.exit(1)
